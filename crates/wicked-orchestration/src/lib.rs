//! wicked-orchestration — event-driven work orchestration on the SHARED wicked-estate store.
//!
//! A Rust port of the Node prototype (`wicked-orchestration/lib/{store,reducer,gate}.mjs`) onto the
//! estate graph: a `Workflow` and a `Phase` are estate [`Node`](wicked_apps_core::Node)s
//! (`kind=Other("workflow"|"phase")`, fields in `metadata`), the reducer is the single writer that
//! advances a phase through the [`ALLOWED_TRANSITIONS`](transitions::ALLOWED_TRANSITIONS) state
//! machine with idempotent at-least-once delivery, and the gate consumes a governance
//! [`ConformanceClaim`](wicked_apps_core::ConformanceClaim) and resolves the phase transition by ADR-0003
//! precedence.
//!
//! ## The structural gate (ADR-0003, the load-bearing invariant)
//! A hard governance `Deny` is PERSISTED on the phase as `gate_decision`. The reducer refuses ANY
//! approving transition (`Approved` / `ApprovedWithConditions`) on a phase carrying that marker —
//! checked BEFORE the transition table — so the falsifier `reject ⇒ ¬approved` holds by any
//! route/race/surface, not merely on the gate's happy path. See
//! [`reducer::apply_event`] (step 1.5) and the `structural_veto_*` tests.
//!
//! ## Modules
//! - [`domain`] — `Workflow`, `Phase`, `PhaseStatus`, and the `ToNode`/`FromNode` projection.
//! - [`transitions`] — the `ALLOWED_TRANSITIONS` state machine.
//! - [`reducer`] — `apply_event`, the single-writer reducer (idempotency + veto + validate + project).
//! - [`gate`] — `resolve_gate` / `apply_gate`, the verdict-consuming gate + coarse emit.
//!
//! Built against wicked-apps-core (the verified estate API spine); does NOT depend on wicked-governance
//! (lane-disjoint) — `ConformanceClaim` is the wicked-apps-core type, constructed directly by callers.

pub mod domain;
pub mod gate;
pub mod reducer;
pub mod runner;
pub mod transitions;

pub use domain::{Phase, PhaseStatus, Workflow, WorkflowStatus};
pub use gate::{apply_gate, resolve_gate, GateOutcome, EV_PHASE_TRANSITIONED};
pub use reducer::{
    apply_event, get_phase, is_processed, put_phase, ApplyOutcome, Event, Transition,
};
pub use runner::{
    advance, create_workflow, get_workflow, register_workflow, tick_workflow, AdvanceOutcome,
};
pub use transitions::{emitted_event_type_for, is_legal_transition, ALLOWED_TRANSITIONS};

/// Crate identity smoke.
pub fn health() -> &'static str {
    "wicked-orchestration"
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::{ConformanceClaim, Decision, FromNode, GraphWrite, SqliteStore, ToNode};

    // ── Test helpers ─────────────────────────────────────────────────────────

    /// A fresh hermetic in-memory estate store.
    fn store() -> SqliteStore {
        SqliteStore::in_memory().expect("open in-memory estate store")
    }

    /// Persist `phase` directly (bypassing the reducer) to set up a test fixture.
    fn seed_phase(s: &mut SqliteStore, phase: &Phase) {
        s.begin_batch().unwrap();
        s.upsert_nodes(&[phase.to_node()]).unwrap();
        s.commit_batch().unwrap();
    }

    /// Build a ConformanceClaim with the given decision + obligations (the gate input). Constructed
    /// directly — this crate does NOT depend on wicked-governance.
    fn claim(decision: Decision, obligations: &[&str]) -> ConformanceClaim {
        ConformanceClaim {
            claim_id: "claim-1".into(),
            scope: "repo:acme".into(),
            phase: "build".into(),
            policy_ids: vec!["pol-1".into()],
            decision,
            obligations: obligations.iter().map(|s| s.to_string()).collect(),
            evaluated_context_ref: "ctx://abc".into(),
            criteria: "all gates green".into(),
            evaluator_identity: "governance@v1".into(),
            evaluated_at: 1_750_000_000,
        }
    }

    /// Drive a freshly-opened phase up to `GateRunning` through legal reducer transitions, so the
    /// gate tests start from the only state the gate fires from.
    fn phase_at_gate_running(s: &mut SqliteStore, phase_id: &str) {
        let p = Phase::open(phase_id, "wf-1", "Build");
        seed_phase(s, &p);
        for (i, to) in [
            PhaseStatus::InProgress,
            PhaseStatus::ReadyForGate,
            PhaseStatus::GateRunning,
        ]
        .into_iter()
        .enumerate()
        {
            let out =
                apply_event(s, &Event::transition(format!("ev-setup-{i}"), phase_id, to)).unwrap();
            assert!(
                out.applied,
                "setup transition to {to:?} must apply: {out:?}"
            );
        }
        assert_eq!(
            get_phase(s, phase_id).unwrap().unwrap().status,
            PhaseStatus::GateRunning
        );
    }

    // ── Phase round-trips through a real SqliteStore ───────────────────────────

    #[test]
    fn phase_round_trips_through_in_memory_store() {
        let original = Phase {
            id: "wf-1:build".into(),
            workflow_id: "wf-1".into(),
            name: "Build".into(),
            status: PhaseStatus::ApprovedWithConditions,
            obligations: vec!["redact:token".into(), "require:human-approval".into()],
            gate_decision: Some(Decision::AllowWithConditions),
        };

        let node = original.to_node();
        let symbol = node.symbol.clone();

        let mut s = store();
        s.begin_batch().unwrap();
        s.upsert_nodes(&[node]).unwrap();
        s.commit_batch().unwrap();

        let fetched = wicked_apps_core::GraphRead::get_node(&s, &symbol)
            .unwrap()
            .expect("phase node present after upsert");
        let recovered = Phase::from_node(&fetched).expect("from_node ok");

        assert_eq!(
            original, recovered,
            "Phase must survive Node round-trip through SqliteStore"
        );
    }

    #[test]
    fn workflow_round_trips_through_in_memory_store() {
        let original = Workflow::new("wf-42");
        let node = original.to_node();
        let symbol = node.symbol.clone();

        let mut s = store();
        s.begin_batch().unwrap();
        s.upsert_nodes(&[node]).unwrap();
        s.commit_batch().unwrap();

        let fetched = wicked_apps_core::GraphRead::get_node(&s, &symbol)
            .unwrap()
            .unwrap();
        let recovered = Workflow::from_node(&fetched).unwrap();
        assert_eq!(original, recovered);
    }

    // ── Transition validation (legal advances; illegal rejected) ───────────────

    #[test]
    fn transition_table_admits_legal_edges_and_rejects_illegal() {
        // A representative legal advance.
        assert!(is_legal_transition(
            PhaseStatus::Pending,
            PhaseStatus::InProgress
        ));
        assert!(is_legal_transition(
            PhaseStatus::GateRunning,
            PhaseStatus::Approved
        ));
        // Skipping is legal from every non-terminal state.
        assert!(is_legal_transition(
            PhaseStatus::ReadyForGate,
            PhaseStatus::Skipped
        ));
        // Illegal: skipping the machine, and any edge out of a terminal state.
        assert!(!is_legal_transition(
            PhaseStatus::Pending,
            PhaseStatus::Approved
        ));
        assert!(!is_legal_transition(
            PhaseStatus::Approved,
            PhaseStatus::Rejected
        ));
        assert!(!is_legal_transition(
            PhaseStatus::Pending,
            PhaseStatus::Pending
        ));
    }

    #[test]
    fn reducer_applies_legal_transition_and_refuses_illegal() {
        let mut s = store();
        let phase_id = "wf-1:build";
        seed_phase(&mut s, &Phase::open(phase_id, "wf-1", "Build"));

        // Legal: pending -> in_progress.
        let out = apply_event(
            &mut s,
            &Event::transition("ev-1", phase_id, PhaseStatus::InProgress),
        )
        .unwrap();
        assert!(out.applied);
        assert_eq!(out.transitions.len(), 1);
        assert_eq!(out.transitions[0].from, PhaseStatus::Pending);
        assert_eq!(out.transitions[0].to, PhaseStatus::InProgress);
        assert_eq!(out.transitions[0].event_type, Some("wicked.crew.phase.started"));
        assert_eq!(
            get_phase(&s, phase_id).unwrap().unwrap().status,
            PhaseStatus::InProgress
        );

        // Illegal: in_progress -> approved (skips the gate). Refused, status unchanged.
        let out = apply_event(
            &mut s,
            &Event::transition("ev-2", phase_id, PhaseStatus::Approved),
        )
        .unwrap();
        assert!(!out.applied);
        assert_eq!(
            out.reason.as_deref(),
            Some("illegal_transition: 'in_progress' -> 'approved'")
        );
        assert_eq!(
            get_phase(&s, phase_id).unwrap().unwrap().status,
            PhaseStatus::InProgress,
            "an illegal transition must not change the projected status"
        );
    }

    #[test]
    fn reducer_rejects_from_mismatch() {
        let mut s = store();
        let phase_id = "wf-1:build";
        seed_phase(&mut s, &Phase::open(phase_id, "wf-1", "Build")); // status = pending

        let ev = Event {
            id: "ev-1".into(),
            phase_id: phase_id.into(),
            to: PhaseStatus::InProgress,
            from: Some(PhaseStatus::ReadyForGate), // wrong assertion
            obligations: None,
            gate_decision: None,
        };
        let out = apply_event(&mut s, &ev).unwrap();
        assert!(!out.applied);
        assert!(out.reason.unwrap().starts_with("from_mismatch:"));
    }

    // ── Idempotency (same event id twice ⇒ second no-op) ───────────────────────

    #[test]
    fn idempotency_same_event_id_twice_is_no_op() {
        let mut s = store();
        let phase_id = "wf-1:build";
        seed_phase(&mut s, &Phase::open(phase_id, "wf-1", "Build"));

        let ev = Event::transition("ev-dup", phase_id, PhaseStatus::InProgress);

        let first = apply_event(&mut s, &ev).unwrap();
        assert!(first.applied, "first application must apply");
        assert!(is_processed(&s, "ev-dup").unwrap());

        // Re-deliver the SAME id. It must be a no-op with reason "duplicate" — even though
        // pending->in_progress would otherwise be... already-applied; the dedup short-circuits
        // before the transition is even examined.
        let second = apply_event(&mut s, &ev).unwrap();
        assert!(!second.applied);
        assert_eq!(second.reason.as_deref(), Some("duplicate"));
        assert!(second.transitions.is_empty());

        // A DIFFERENT id that would now be an illegal repeat (in_progress->in_progress) is refused
        // for the RIGHT reason (illegal), proving the dedup is keyed on id, not on the target.
        let other = apply_event(
            &mut s,
            &Event::transition("ev-other", phase_id, PhaseStatus::InProgress),
        )
        .unwrap();
        assert!(!other.applied);
        assert_eq!(
            other.reason.as_deref(),
            Some("illegal_transition: 'in_progress' -> 'in_progress'")
        );
    }

    // ── Gate mapping: Deny / AllowWithConditions / Allow / none ────────────────

    #[test]
    fn gate_deny_resolves_to_rejected() {
        let mut s = store();
        let phase_id = "p-deny";
        phase_at_gate_running(&mut s, phase_id);

        let c = claim(Decision::Deny, &[]);
        let out = apply_gate(&mut s, phase_id, Some(&c), "gate-1").unwrap();
        assert_eq!(out.resolved, PhaseStatus::Rejected);
        assert!(out.applied);
        let phase = get_phase(&s, phase_id).unwrap().unwrap();
        assert_eq!(phase.status, PhaseStatus::Rejected);
        assert_eq!(phase.gate_decision, Some(Decision::Deny));
    }

    #[test]
    fn gate_allow_with_conditions_resolves_with_obligations_on_phase() {
        let mut s = store();
        let phase_id = "p-awc";
        phase_at_gate_running(&mut s, phase_id);

        let c = claim(
            Decision::AllowWithConditions,
            &["redact:token", "require:human-approval"],
        );
        let out = apply_gate(&mut s, phase_id, Some(&c), "gate-1").unwrap();
        assert_eq!(out.resolved, PhaseStatus::ApprovedWithConditions);
        assert!(out.applied);
        assert!(
            out.conditions,
            "approved_with_conditions must flag conditions=true"
        );
        assert_eq!(
            out.obligations,
            vec!["redact:token", "require:human-approval"]
        );

        let phase = get_phase(&s, phase_id).unwrap().unwrap();
        assert_eq!(phase.status, PhaseStatus::ApprovedWithConditions);
        assert_eq!(
            phase.obligations,
            vec![
                "redact:token".to_string(),
                "require:human-approval".to_string()
            ],
            "obligations from the claim must be carried ONTO the phase"
        );
        assert_eq!(phase.gate_decision, Some(Decision::AllowWithConditions));
    }

    #[test]
    fn gate_allow_resolves_to_approved() {
        let mut s = store();
        let phase_id = "p-allow";
        phase_at_gate_running(&mut s, phase_id);

        let c = claim(Decision::Allow, &[]);
        let out = apply_gate(&mut s, phase_id, Some(&c), "gate-1").unwrap();
        assert_eq!(out.resolved, PhaseStatus::Approved);
        assert!(out.applied);
        assert!(!out.conditions);
        assert_eq!(
            get_phase(&s, phase_id).unwrap().unwrap().status,
            PhaseStatus::Approved
        );
    }

    #[test]
    fn gate_no_claim_stays_gate_running_never_silent_approve() {
        let mut s = store();
        let phase_id = "p-none";
        phase_at_gate_running(&mut s, phase_id);

        let out = apply_gate(&mut s, phase_id, None, "gate-1").unwrap();
        assert_eq!(out.resolved, PhaseStatus::GateRunning);
        assert!(!out.applied);
        assert_eq!(out.reason.as_deref(), Some("no_claim"));
        assert_eq!(
            get_phase(&s, phase_id).unwrap().unwrap().status,
            PhaseStatus::GateRunning,
            "absence of a verdict must NEVER silent-approve — it stays gate_running"
        );
    }

    // ── Runner: multi-stage workflow advance ──────────────────────────────────────

    /// Three-phase workflow walks all the way through: each phase is driven to Approved via
    /// the gate, then `advance` moves the cursor and opens the next phase.
    #[test]
    fn runner_three_phase_workflow_advances_to_complete() {
        let mut s = store();
        let wf = create_workflow(
            &mut s,
            "wf-run",
            "Run",
            &[
                ("wf-run:design", "Design"),
                ("wf-run:build", "Build"),
                ("wf-run:review", "Review"),
            ],
        )
        .unwrap();
        assert_eq!(wf.phases.len(), 3);
        assert_eq!(wf.current_index, 0);

        // Phase 0 is InProgress after create_workflow.
        let p0 = get_phase(&s, "wf-run:design").unwrap().unwrap();
        assert_eq!(p0.status, PhaseStatus::InProgress);

        // Drive phase 0 through the gate to Approved.
        phase_gate_to_approved(&mut s, "wf-run:design", "gate-0");
        let out = advance(&mut s, "wf-run").unwrap();
        assert!(
            matches!(&out, AdvanceOutcome::Advanced { to_phase_id, .. } if to_phase_id == "wf-run:build"),
            "expected Advanced to build, got {out:?}"
        );

        // Phase 1 is open.
        assert_eq!(
            get_phase(&s, "wf-run:build").unwrap().unwrap().status,
            PhaseStatus::InProgress
        );
        assert_eq!(
            get_workflow(&s, "wf-run").unwrap().unwrap().current_index,
            1
        );

        // Drive phase 1.
        phase_gate_to_approved(&mut s, "wf-run:build", "gate-1");
        let out = advance(&mut s, "wf-run").unwrap();
        assert!(
            matches!(&out, AdvanceOutcome::Advanced { to_phase_id, .. } if to_phase_id == "wf-run:review")
        );

        // Drive final phase.
        phase_gate_to_approved(&mut s, "wf-run:review", "gate-2");
        let out = advance(&mut s, "wf-run").unwrap();
        assert_eq!(out, AdvanceOutcome::Complete);

        let wf_final = get_workflow(&s, "wf-run").unwrap().unwrap();
        assert_eq!(wf_final.status, WorkflowStatus::Complete);
    }

    /// A rejected phase halts the workflow — subsequent `advance` calls return `Failed`.
    #[test]
    fn runner_rejected_phase_halts_workflow() {
        let mut s = store();
        create_workflow(
            &mut s,
            "wf-reject",
            "Reject",
            &[("wf-reject:build", "Build"), ("wf-reject:deploy", "Deploy")],
        )
        .unwrap();

        // Phase is InProgress after create_workflow. Drive to GateRunning then deny.
        apply_event(
            &mut s,
            &Event::transition("rfg-r", "wf-reject:build", PhaseStatus::ReadyForGate),
        )
        .unwrap();
        apply_event(
            &mut s,
            &Event::transition("gr-r", "wf-reject:build", PhaseStatus::GateRunning),
        )
        .unwrap();
        apply_gate(
            &mut s,
            "wf-reject:build",
            Some(&claim(Decision::Deny, &[])),
            "gate-deny",
        )
        .unwrap();

        let out = advance(&mut s, "wf-reject").unwrap();
        assert_eq!(
            out,
            AdvanceOutcome::Failed {
                phase_id: "wf-reject:build".into()
            }
        );
        // Calling advance again returns the same Failed outcome (idempotent).
        let out2 = advance(&mut s, "wf-reject").unwrap();
        assert_eq!(out2, out);
        // Phase 1 was never opened.
        assert!(get_phase(&s, "wf-reject:deploy").unwrap().is_none());
    }

    /// A phase awaiting deliverables pauses the workflow without advancing the cursor.
    #[test]
    fn runner_awaiting_deliverables_pauses_workflow() {
        let mut s = store();
        create_workflow(
            &mut s,
            "wf-await",
            "Await",
            &[("wf-await:design", "Design"), ("wf-await:build", "Build")],
        )
        .unwrap();

        // Advance phase to AwaitingDeliverables via the reducer.
        apply_event(
            &mut s,
            &Event::transition(
                "ev-await",
                "wf-await:design",
                PhaseStatus::AwaitingDeliverables,
            ),
        )
        .unwrap();

        let out = advance(&mut s, "wf-await").unwrap();
        assert_eq!(
            out,
            AdvanceOutcome::AwaitingHuman {
                phase_id: "wf-await:design".into()
            }
        );
        assert_eq!(
            get_workflow(&s, "wf-await").unwrap().unwrap().status,
            WorkflowStatus::AwaitingHuman
        );
        // Cursor did not move; phase 1 not opened.
        assert_eq!(
            get_workflow(&s, "wf-await").unwrap().unwrap().current_index,
            0
        );
        assert!(get_phase(&s, "wf-await:build").unwrap().is_none());
    }

    /// Empty workflow is immediately Complete.
    #[test]
    fn runner_empty_workflow_is_complete() {
        let mut s = store();
        let wf = create_workflow(&mut s, "wf-empty", "Empty", &[] as &[(&str, &str)]).unwrap();
        assert_eq!(wf.status, WorkflowStatus::Complete);
        let out = advance(&mut s, "wf-empty").unwrap();
        assert_eq!(out, AdvanceOutcome::Complete);
    }

    // Helpers for runner tests ─────────────────────────────────────────────────

    /// Drive a phase from InProgress all the way through to gate_running, then Approved.
    fn phase_gate_to_approved(s: &mut SqliteStore, phase_id: &str, gate_event_id: &str) {
        // InProgress → ReadyForGate → GateRunning (reducer transitions)
        apply_event(
            s,
            &Event::transition(
                format!("{gate_event_id}-rfg"),
                phase_id,
                PhaseStatus::ReadyForGate,
            ),
        )
        .unwrap();
        apply_event(
            s,
            &Event::transition(
                format!("{gate_event_id}-gr"),
                phase_id,
                PhaseStatus::GateRunning,
            ),
        )
        .unwrap();
        let c = claim(Decision::Allow, &[]);
        apply_gate(s, phase_id, Some(&c), gate_event_id).unwrap();
        assert_eq!(
            get_phase(s, phase_id).unwrap().unwrap().status,
            PhaseStatus::Approved
        );
    }

    // ── STRUCTURAL FALSIFIER (ADR-0003): reject ⇒ ¬approved by ANY route ───────

    /// After a Deny gate persists `gate_decision = Deny`, a RAW `apply_event` transition to Approved
    /// is REFUSED with `vetoed_by_governance`, and the phase is NOT Approved.
    ///
    /// This FAILS if the veto lived only in the gate's happy-path mapping (`resolve_gate`): the raw
    /// event bypasses `resolve_gate` entirely and goes straight at the reducer. Only the PERSISTED
    /// marker + the reducer's step-1.5 structural check can refuse it.
    #[test]
    fn structural_veto_raw_approve_after_deny_is_refused() {
        let mut s = store();
        let phase_id = "p-falsify";
        phase_at_gate_running(&mut s, phase_id);

        // Governance denies. Phase resolves to Rejected and the Deny marker is persisted.
        let c = claim(Decision::Deny, &[]);
        let denied = apply_gate(&mut s, phase_id, Some(&c), "gate-1").unwrap();
        assert_eq!(denied.resolved, PhaseStatus::Rejected);
        assert_eq!(
            get_phase(&s, phase_id).unwrap().unwrap().gate_decision,
            Some(Decision::Deny)
        );

        // A RAW reducer event tries to force Approved (a stale/racing/malicious command). The state
        // machine has NO rejected->approved edge anyway, so to PROVE the veto (not just the table)
        // we craft an event that asserts the from-state and targets an approving status. The veto
        // (step 1.5) must fire BEFORE the transition table, with reason `vetoed_by_governance`.
        let raw = Event {
            id: "raw-approve".into(),
            phase_id: phase_id.into(),
            to: PhaseStatus::Approved,
            from: Some(PhaseStatus::GateRunning), // claim the gate is still running
            obligations: None,
            gate_decision: None,
        };
        let out = apply_event(&mut s, &raw).unwrap();
        assert!(!out.applied, "an approve on a denied phase must be refused");
        assert_eq!(
            out.reason.as_deref(),
            Some("vetoed_by_governance"),
            "the refusal must be the STRUCTURAL veto, not merely an illegal-transition / from-mismatch"
        );

        let phase = get_phase(&s, phase_id).unwrap().unwrap();
        assert_ne!(
            phase.status,
            PhaseStatus::Approved,
            "reject ⇒ ¬approved: the phase must NOT be Approved by any route"
        );
        assert_eq!(phase.status, PhaseStatus::Rejected);
    }

    /// A second angle on the falsifier: even a phase that is STILL in `gate_running` (never resolved)
    /// but already carries a persisted `Deny` cannot be raw-driven to `ApprovedWithConditions`. This
    /// isolates the veto from the rejected-terminal-state coincidence — the only thing forbidding the
    /// otherwise-LEGAL `gate_running -> approved_with_conditions` edge is the persisted marker.
    #[test]
    fn structural_veto_blocks_legal_gate_edge_when_decision_denies() {
        let mut s = store();
        let phase_id = "p-falsify-2";
        phase_at_gate_running(&mut s, phase_id);

        // Persist a Deny marker WITHOUT resolving the phase, by driving a raw event that stays in
        // gate_running is impossible (no self-edge); instead set the marker via a from-asserted
        // gate_running->gate_running... also impossible. So persist it directly on the phase node:
        let mut p = get_phase(&s, phase_id).unwrap().unwrap();
        assert_eq!(p.status, PhaseStatus::GateRunning);
        p.gate_decision = Some(Decision::Deny);
        put_phase(&mut s, &p).unwrap();

        // The edge gate_running -> approved_with_conditions IS in the transition table (legal), so a
        // pure table check would let this through. The structural veto must still refuse it.
        let raw = Event {
            id: "raw-awc".into(),
            phase_id: phase_id.into(),
            to: PhaseStatus::ApprovedWithConditions,
            from: Some(PhaseStatus::GateRunning),
            obligations: None,
            gate_decision: None,
        };
        // Sanity: the edge really is legal in the table (so this test is non-vacuous).
        assert!(is_legal_transition(
            PhaseStatus::GateRunning,
            PhaseStatus::ApprovedWithConditions
        ));

        let out = apply_event(&mut s, &raw).unwrap();
        assert!(!out.applied);
        assert_eq!(out.reason.as_deref(), Some("vetoed_by_governance"));
        assert_eq!(
            get_phase(&s, phase_id).unwrap().unwrap().status,
            PhaseStatus::GateRunning,
            "the denied phase must remain gate_running — the legal approving edge is structurally unavailable"
        );
    }

    // ── register_workflow / tick_workflow ─────────────────────────────────────

    /// `register_workflow` persists the workflow node but does NOT open any phases.
    #[test]
    fn register_workflow_creates_node_without_opening_phases() {
        let mut s = store();
        let wf = register_workflow(
            &mut s,
            "wf-reg",
            "Reg",
            &[
                ("wf-reg:p0", "Phase 0"),
                ("wf-reg:p1", "Phase 1"),
                ("wf-reg:p2", "Phase 2"),
            ],
        )
        .unwrap();

        // Workflow node must be persisted.
        let stored = get_workflow(&s, "wf-reg")
            .unwrap()
            .expect("workflow node present");
        assert_eq!(stored.id, wf.id);
        assert_eq!(stored.phases.len(), 3);
        assert_eq!(stored.current_index, 0);
        assert_eq!(stored.status, WorkflowStatus::Running);

        // None of the 3 phase nodes should exist — register_workflow does NOT open them.
        assert!(
            get_phase(&s, "wf-reg:p0").unwrap().is_none(),
            "p0 must not be opened"
        );
        assert!(
            get_phase(&s, "wf-reg:p1").unwrap().is_none(),
            "p1 must not be opened"
        );
        assert!(
            get_phase(&s, "wf-reg:p2").unwrap().is_none(),
            "p2 must not be opened"
        );
    }

    /// `tick_workflow` advances the cursor through all phases and marks Complete on the last tick.
    #[test]
    fn tick_workflow_advances_cursor_through_all_units() {
        let mut s = store();
        register_workflow(
            &mut s,
            "wf-tick",
            "Tick",
            &[
                ("wf-tick:p0", "Phase 0"),
                ("wf-tick:p1", "Phase 1"),
                ("wf-tick:p2", "Phase 2"),
            ],
        )
        .unwrap();

        // Tick 1 — move from index 0 to 1.
        let status = tick_workflow(&mut s, "wf-tick", true).unwrap();
        assert_eq!(status, WorkflowStatus::Running);
        let wf = get_workflow(&s, "wf-tick").unwrap().unwrap();
        assert_eq!(wf.current_index, 1);
        assert_eq!(wf.status, WorkflowStatus::Running);

        // Tick 2 — move from index 1 to 2.
        let status = tick_workflow(&mut s, "wf-tick", true).unwrap();
        assert_eq!(status, WorkflowStatus::Running);
        let wf = get_workflow(&s, "wf-tick").unwrap().unwrap();
        assert_eq!(wf.current_index, 2);
        assert_eq!(wf.status, WorkflowStatus::Running);

        // Tick 3 — last phase; must become Complete.
        let status = tick_workflow(&mut s, "wf-tick", true).unwrap();
        assert_eq!(status, WorkflowStatus::Complete);
        let wf = get_workflow(&s, "wf-tick").unwrap().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Complete);
    }

    /// A rejected tick marks the workflow Failed, and subsequent ticks are idempotent (index stays).
    #[test]
    fn tick_workflow_rejected_marks_failed_and_is_idempotent() {
        let mut s = store();
        register_workflow(
            &mut s,
            "wf-fail",
            "Fail",
            &[
                ("wf-fail:p0", "Phase 0"),
                ("wf-fail:p1", "Phase 1"),
                ("wf-fail:p2", "Phase 2"),
            ],
        )
        .unwrap();

        // Approve first phase (cursor 0 → 1).
        tick_workflow(&mut s, "wf-fail", true).unwrap();
        let wf = get_workflow(&s, "wf-fail").unwrap().unwrap();
        assert_eq!(wf.current_index, 1);

        // Reject second phase — workflow must be Failed at index 1.
        let status = tick_workflow(&mut s, "wf-fail", false).unwrap();
        assert_eq!(status, WorkflowStatus::Failed);
        let wf = get_workflow(&s, "wf-fail").unwrap().unwrap();
        assert_eq!(wf.status, WorkflowStatus::Failed);
        assert_eq!(wf.current_index, 1, "index must not advance on rejection");

        // Additional tick (approved) on a failed workflow — idempotent, index unchanged.
        let status2 = tick_workflow(&mut s, "wf-fail", true).unwrap();
        assert_eq!(
            status2,
            WorkflowStatus::Failed,
            "failed workflow stays failed"
        );
        let wf2 = get_workflow(&s, "wf-fail").unwrap().unwrap();
        assert_eq!(
            wf2.current_index, 1,
            "index must not change after tick on failed workflow"
        );
    }

    /// `tick_workflow` and `advance` are independent; calling `advance` when phases were never
    /// opened (register_workflow path) returns an error and leaves the workflow state unchanged.
    #[test]
    fn tick_workflow_and_advance_are_independent() {
        let mut s = store();
        register_workflow(
            &mut s,
            "wf-ind",
            "Ind",
            &[("wf-ind:p0", "Phase 0"), ("wf-ind:p1", "Phase 1")],
        )
        .unwrap();

        // Tick once approved — cursor moves to 1, status Running.
        let status = tick_workflow(&mut s, "wf-ind", true).unwrap();
        assert_eq!(status, WorkflowStatus::Running);
        let wf_before = get_workflow(&s, "wf-ind").unwrap().unwrap();
        assert_eq!(wf_before.current_index, 1);

        // advance reads the phase at index 1 ("wf-ind:p1"), which was never opened → returns Err.
        // The workflow state must NOT be mutated by the failed advance.
        let advance_result = advance(&mut s, "wf-ind");
        assert!(
            advance_result.is_err(),
            "advance on an un-opened phase must return Err, got: {advance_result:?}"
        );

        // Workflow is unchanged after the failed advance.
        let wf_after = get_workflow(&s, "wf-ind").unwrap().unwrap();
        assert_eq!(
            wf_after.current_index, 1,
            "index must not change after failed advance"
        );
        assert_eq!(
            wf_after.status,
            WorkflowStatus::Running,
            "status must not change after failed advance"
        );
    }
}
