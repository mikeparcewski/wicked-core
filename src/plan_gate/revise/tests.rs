//! T4 (§8.7) on the pure pipeline: the floor raise and its bands, the ratchet, late phases,
//! the approval matrix for revisions, and the PA's `PLAN` line grammar. Expected bands and floor
//! phases are literals from DES-TEAMING-002 §8.5's table.

use serde_json::{json, Value};

use super::*;
use crate::plan_gate::{decide, Proposal, Verdict};
use crate::review_scale::{plan_for, Assessment};
use crate::team::events::{TeamBody, PLAN_PROPOSED, PLAN_REVISED};

fn plan(v: Value) -> PlanSteps {
    serde_json::from_value(v).unwrap()
}

fn assessment(score: u8) -> Assessment {
    Assessment {
        deterministic: score,
        score,
        reasons: vec![format!("fixture {score}")],
        model: None,
        signals: None,
        plan: plan_for(score),
    }
}

/// The run's state after its launch plan was accepted at `score` (auto mode).
fn accepted(steps: Value, score: u8, hc: &HumanConfirm) -> TeamPlanState {
    let d = decide(
        "r",
        Proposal {
            by: "human".into(),
            source: ProposalSource::Launch {
                session_id: "r".into(),
            },
            kind: ProposalKind::Initial,
            preset: None,
            plan: plan(json!({ "steps": steps, "touch": ["src/x.rs"] })),
            reviewing_ord: None,
            approved_by_human: false,
        },
        &TeamPlanState::default(),
        hc,
        &Scored {
            assessment: assessment(score),
            destructive: false,
        },
        0,
    )
    .unwrap();
    match d.verdict {
        Verdict::Accepted { .. } => d.state,
        _ => {
            // A held initial plan (manual mode) — approve it as the gate would.
            let mut s = d.state;
            let p = s.pending.take().unwrap();
            s.accepted = Some(AcceptedPlan {
                rev: p.rev,
                by: "human".into(),
                band: p.band,
                high_risk: p.high_risk,
                auto: is_auto(hc),
                steps: p.steps,
                floor_override: None,
                proposal_id: p.proposal_id,
            });
            s.accepted_rev = p.rev;
            s.accepted_high_risk = p.high_risk;
            s
        }
    }
}

fn floor(score: u8) -> Change {
    let fact = path_scored_diff("r", 2, 0, 1, Some("t"), &assessment(score), 0).unwrap();
    Change::Floor(DiffRescore {
        ord: 2,
        attempt: 0,
        rescore_seq: 1,
        score,
        destructive: false,
        fact: crate::plan_gate::queued_facts(&[fact]).unwrap().pop().unwrap(),
    })
}

fn revised(r: &Revised) -> crate::team::events::PlanRevised {
    r.events
        .iter()
        .find_map(|e| match &e.body {
            TeamBody::PlanRevised(b) => Some(b.clone()),
            _ => None,
        })
        .expect("plan.revised")
}

fn def_ids(r: &Revised) -> Vec<String> {
    match &r.outcome {
        Outcome::Accepted { def } | Outcome::Held { def } => {
            def.phases.iter().map(|p| p.id.clone()).collect()
        }
        Outcome::Refused { reason } => panic!("refused: {reason}"),
    }
}

/// (a) A diff re-score from band 20–39 into 40–69 publishes `path.scored{basis:"diff"}` then
/// `plan.revised{reason:"floor_raised", added:[test_plan, design]}`, inserted after the cursor.
#[test]
fn t4_a_a_rescore_into_40_69_adds_test_plan_and_design_after_the_cursor() {
    let hc = HumanConfirm::None;
    let s = accepted(
        json!([{"catalog":"understand"},{"catalog":"build"},{"catalog":"review"}]),
        25,
        &hc,
    );
    assert_eq!(s.accepted.as_ref().unwrap().band, "20-39");
    assert!(floor_rises(&s, 50, false));
    let r = revise("r", &s, floor(50), &["understand".into()], &hc, Some(1), 0).unwrap();
    let types: Vec<&str> = r.events.iter().map(|e| e.event_type()).collect();
    assert_eq!(types, [crate::team::events::PATH_SCORED, PLAN_REVISED]);
    let b = revised(&r);
    assert_eq!(b.reason, ReviseReason::FloorRaised);
    assert_eq!((b.from_band.as_str(), b.to_band.as_str()), ("20-39", "40-69"));
    assert!(!b.high_risk);
    assert_eq!(b.proposal_id, None);
    let added: Vec<(&str, Option<bool>)> = b
        .added
        .iter()
        .map(|s| (s.catalog.as_str(), s.late))
        .collect();
    assert_eq!(added, [("test_plan", Some(false)), ("design", Some(false))]);
    assert_eq!(
        def_ids(&r),
        ["understand", "test_plan", "design", "build", "review"]
    );
    // Auto mode, below high risk: accepted as rev 2 with no gate.
    assert!(matches!(r.outcome, Outcome::Accepted { .. }));
    assert_eq!((r.state.rev, r.state.accepted_rev), (2, 2));
    assert_eq!(r.state.max_score, 50);
}

/// (a) second clause: the band only ratchets up — a later lower (or same-band) score does not
/// rise, so nothing is held and nothing is published.
#[test]
fn t4_a_a_lower_or_same_band_score_does_not_rise() {
    let hc = HumanConfirm::None;
    let mut s = accepted(json!([{"catalog":"build"},{"catalog":"review"}]), 25, &hc);
    s.max_score = 50;
    assert!(!floor_rises(&s, 10, false));
    assert!(!floor_rises(&s, 45, false), "same band 40-69");
    assert!(floor_rises(&s, 70, false));
    assert!(floor_rises(&s, 45, true), "destructive is high risk in any band");
}

/// (b) Into high risk in auto mode holds the revision (`into_high_risk`); in manual mode every
/// revision is held, even below high risk.
#[test]
fn t4_b_into_high_risk_holds_in_auto_and_every_revision_holds_in_manual() {
    let auto = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"build"},{"catalog":"review"}]), 25, &auto);
    let r = revise("r", &s, floor(80), &["build".into()], &auto, Some(1), 0).unwrap();
    assert!(matches!(r.outcome, Outcome::Held { .. }));
    let p = r.state.pending.as_ref().unwrap();
    assert_eq!(p.reason, "into_high_risk");
    assert_eq!(p.reviewing_ord, Some(1));
    assert!(p.high_risk);
    assert_eq!(r.state.accepted_rev, 1, "nothing accepted until the gate is answered");
    let manual = HumanConfirm::Before(99);
    let s = accepted(json!([{"catalog":"build"},{"catalog":"review"}]), 25, &manual);
    let r = revise("r", &s, floor(50), &["build".into()], &manual, Some(1), 0).unwrap();
    assert!(matches!(r.outcome, Outcome::Held { .. }));
    assert_eq!(r.state.pending.as_ref().unwrap().reason, "manual_mode");
}

/// (c) A floor phase whose catalog position precedes a done step lands at the cursor, `late`.
#[test]
fn t4_c_a_floor_phase_before_a_done_step_is_late_at_the_cursor() {
    let hc = HumanConfirm::None;
    let s = accepted(
        json!([{"catalog":"understand"},{"catalog":"build"},{"catalog":"review"}]),
        25,
        &hc,
    );
    let done = ["understand".to_string(), "build".to_string()];
    let r = revise("r", &s, floor(50), &done, &hc, Some(2), 0).unwrap();
    let b = revised(&r);
    let late: Vec<(&str, Option<bool>)> = b
        .added
        .iter()
        .map(|s| (s.catalog.as_str(), s.late))
        .collect();
    assert_eq!(late, [("test_plan", Some(true)), ("design", Some(true))]);
    // The done prefix exactly as it ran, then the late phases at the cursor.
    assert_eq!(
        def_ids(&r),
        ["understand", "build", "test_plan", "design", "review"]
    );
    // The logical plan (the state's) keeps catalog order, so a later floor fill still composes.
    let logical: Vec<&str> = r
        .state
        .accepted
        .as_ref()
        .unwrap()
        .steps
        .steps
        .iter()
        .map(|s| s.id.as_str())
        .collect();
    assert_eq!(logical, ["understand", "test_plan", "design", "build", "review"]);
    let r2 = revise("r", &r.state, floor(90), &done, &hc, Some(2), 0).unwrap();
    assert!(matches!(r2.outcome, Outcome::Held { .. }), "into high risk");
}

/// (d) A PA `PLAN+` on a user plan: `plan.proposed{kind:"change", by:<PA>}` then
/// `plan.revised{reason:"pa_added"}` citing it; the user's steps all present, in order.
#[test]
fn t4_d_a_pa_plan_block_on_a_user_plan() {
    let hc = HumanConfirm::None;
    let s = accepted(
        json!([{"catalog":"build","id":"impl"},{"catalog":"review","id":"check"}]),
        25,
        &hc,
    );
    let changes = changes_from_output(
        "done\nPLAN+ {\"steps\":[{\"catalog\":\"security_review\"}],\"reason\":\"auth\"}\n",
        "claude#1",
        1,
        0,
    );
    assert_eq!(changes.len(), 1);
    let change = changes.into_iter().next().unwrap();
    let r = revise("r", &s, change, &["impl".into()], &hc, Some(1), 0).unwrap();
    let types: Vec<&str> = r.events.iter().map(|e| e.event_type()).collect();
    assert_eq!(types, [PLAN_PROPOSED, PLAN_REVISED]);
    let pid = match &r.events[0].body {
        TeamBody::PlanProposed(p) => {
            assert_eq!(p.kind, ProposalKind::Change);
            assert_eq!(p.base_rev, Some(1));
            p.proposal_id.clone()
        }
        _ => unreachable!(),
    };
    assert_eq!(r.events[0].env.by, "claude#1");
    let b = revised(&r);
    assert_eq!(b.reason, ReviseReason::PaAdded);
    assert_eq!(b.proposal_id.as_deref(), Some(pid.as_str()));
    assert_eq!(def_ids(&r), ["impl", "check", "security_review"]);
}

/// (d) Two proposals against the same base rev have distinct proposal ids, and compose into two
/// successive revisions with both additions kept.
#[test]
fn t4_d_two_proposals_on_one_base_rev_are_two_revisions() {
    let hc = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"build"},{"catalog":"review"}]), 25, &hc);
    let mut changes = changes_from_output(
        "PLAN+ {\"steps\":[{\"catalog\":\"security_review\"}]}\n\
         PLAN+ {\"steps\":[{\"catalog\":\"test\"}]}",
        "a",
        1,
        0,
    );
    assert_eq!(changes.len(), 2);
    let second = changes.pop().unwrap();
    let first = changes.pop().unwrap();
    let r1 = revise("r", &s, first, &["build".into()], &hc, Some(1), 0).unwrap();
    let r2 = revise("r", &r1.state, second, &["build".into()], &hc, Some(1), 0).unwrap();
    let pid = |r: &Revised| match &r.events[0].body {
        TeamBody::PlanProposed(p) => p.proposal_id.clone(),
        _ => unreachable!(),
    };
    assert_ne!(pid(&r1), pid(&r2));
    assert_eq!((revised(&r1).plan_rev, revised(&r2).plan_rev), (2, 3));
    let ids = def_ids(&r2);
    assert!(ids.contains(&"security_review".to_string()) && ids.contains(&"test".to_string()));
}

/// A proposal that adds nothing is refused (`plan.refused`), and a plan never loses a done step.
#[test]
fn t4_a_proposal_adding_nothing_is_refused() {
    let hc = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"build"},{"catalog":"review"}]), 25, &hc);
    let change = Change::Steps {
        by: "a".into(),
        source: ProposalSource::PlanBlock {
            ord: 1,
            attempt: 0,
            plan_block_seq: 1,
        },
        kind: ProposalKind::Change,
        reason: Some(ReviseReason::PaAdded),
        steps: plan(json!({"steps":[{"catalog":"review","id":"review"}]})).steps,
    };
    let r = revise("r", &s, change, &["build".into()], &hc, Some(1), 0).unwrap();
    assert!(matches!(r.outcome, Outcome::Refused { .. }));
    assert_eq!(r.state, s);
    assert!(running_order(&s.accepted.unwrap().steps, &["gone".into()]).is_err());
}

/// (e) The PA's answer grammar: `PLAN <id>: ACCEPT` then `PLAN+` is the member's request (sourced
/// by its change id); a DECLINE, or a `PLAN+` with no answer before it, is the PA's own; a line
/// whose JSON is not a step list is not a block.
#[test]
fn t4_e_the_plan_line_grammar() {
    let out = "PLAN c-1: ACCEPT — yes\n\
               PLAN+ {\"steps\":[{\"catalog\":\"test\"}]}\n\
               PLAN c-2: DECLINE — no\n\
               PLAN+ {\"steps\":[{\"catalog\":\"design\"}]}\n\
               PLAN+ not json\n\
               PLAN+ {\"steps\":[]}\n";
    let c = changes_from_output(out, "a", 3, 1);
    assert_eq!(c.len(), 2);
    match &c[0] {
        Change::Steps { source, reason, .. } => {
            assert_eq!(
                source,
                &ProposalSource::Change {
                    change_id: "c-1".into()
                }
            );
            assert_eq!(reason, &Some(ReviseReason::MemberRequest));
        }
        Change::Floor(_) => unreachable!(),
    }
    match &c[1] {
        Change::Steps { source, reason, .. } => {
            assert_eq!(
                source,
                &ProposalSource::PlanBlock {
                    ord: 3,
                    attempt: 1,
                    plan_block_seq: 2
                }
            );
            assert_eq!(reason, &Some(ReviseReason::PaAdded));
        }
        Change::Floor(_) => unreachable!(),
    }
}
