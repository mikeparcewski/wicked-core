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
        &crate::plan_gate::NoRules,
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
                touch: Vec::new(),
                touch_truncated: false,
                touch_source: None,
                rules: Vec::new(),
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
        fact: crate::plan_gate::queued_facts(&[fact])
            .unwrap()
            .pop()
            .unwrap(),
        obligations: Vec::new(),
        recalled: Vec::new(),
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
    assert_eq!(
        (b.from_band.as_str(), b.to_band.as_str()),
        ("20-39", "40-69")
    );
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
    assert!(
        floor_rises(&s, 45, true),
        "destructive is high risk in any band"
    );
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
    assert_eq!(
        r.state.accepted_rev, 1,
        "nothing accepted until the gate is answered"
    );
    let manual = HumanConfirm::Before(99);
    let s = accepted(
        json!([{"catalog":"build"},{"catalog":"review"}]),
        25,
        &manual,
    );
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
    assert_eq!(
        logical,
        ["understand", "test_plan", "design", "build", "review"]
    );
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
        touch: None,
        scoring: None,
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

/// TR-W1a (Copilot review on #693): a revision of an accepted rev persisted BEFORE the touch
/// fields existed keeps the inherited touch, sourced `user` — never a non-empty touch labelled
/// `none`.
#[test]
fn w1a_a_revision_of_a_pre_w1a_accepted_rev_keeps_its_touch_as_user() {
    let hc = HumanConfirm::None;
    let mut s = accepted(
        json!([{"catalog":"understand"},{"catalog":"build"},{"catalog":"review"}]),
        25,
        &hc,
    );
    // Strip the stamp the way a row persisted before W1a reads back.
    let a = s.accepted.as_mut().unwrap();
    a.touch = Vec::new();
    a.touch_truncated = false;
    a.touch_source = None;
    let r = revise("r", &s, floor(50), &["understand".into()], &hc, Some(1), 0).unwrap();
    assert!(matches!(r.outcome, Outcome::Accepted { .. }));
    let acc = r.state.accepted.as_ref().unwrap();
    assert_eq!(acc.touch, ["src/x.rs"]);
    assert_eq!(acc.touch_source, Some(TouchSource::User));
}

/// WT-C3 (DES-walkthrough-proof §4.12): a diff re-score that newly fires a held rule, with no
/// band change, revises the plan through the same `floor_raised`: the rule's pair is added after
/// the cursor with its `floor_rule`, the obligation is ratcheted onto the run's state, and the
/// accepted rev records the rule `applied`.
#[test]
fn wt_c3_a_rescore_that_newly_fires_a_held_rule_raises_the_floor_with_its_pair() {
    let hc = HumanConfirm::None;
    let s = accepted(
        json!([{"catalog":"understand"},{"catalog":"build"},{"catalog":"review"}]),
        25,
        &hc,
    );
    assert!(s.obligations.is_empty());
    let Change::Floor(mut r) = floor(25) else {
        unreachable!()
    };
    r.obligations = vec![crate::plan::HeldObligation {
        rule: "TST-1002".into(),
        token: "step:walkthrough".into(),
    }];
    let out = revise(
        "r",
        &s,
        Change::Floor(r),
        &["understand".into(), "build".into()],
        &hc,
        Some(2),
        0,
    )
    .unwrap();
    let b = revised(&out);
    assert_eq!(b.reason, ReviseReason::FloorRaised);
    assert_eq!(
        (b.from_band.as_str(), b.to_band.as_str()),
        ("20-39", "20-39")
    );
    let added: Vec<(&str, Option<&str>)> = b
        .added
        .iter()
        .map(|s| (s.catalog.as_str(), s.floor_rule.as_deref()))
        .collect();
    assert_eq!(
        added,
        [
            ("walkthrough_plan", Some("TST-1002")),
            ("walkthrough_review", Some("TST-1002"))
        ]
    );
    assert_eq!(
        def_ids(&out),
        [
            "understand",
            "build",
            "walkthrough_plan",
            "walkthrough_review",
            "review"
        ]
    );
    assert_eq!(out.state.obligations.len(), 1);
    let rules = &out.state.accepted.as_ref().unwrap().rules;
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].id, "TST-1002");
}

/// WT-C3 (§4.8): a PA-added creator after a walkthrough that already ran gets a new pair after
/// it (the plan never removes the done one).
#[test]
fn wt_c3_a_creator_added_after_a_done_walkthrough_gets_a_new_pair() {
    let hc = HumanConfirm::None;
    let s = accepted(
        json!([{"catalog":"build"},{"catalog":"walkthrough_plan"},{"catalog":"walkthrough_review"},
               {"catalog":"review"}]),
        25,
        &hc,
    );
    let done: Vec<String> = ["build", "walkthrough_plan", "walkthrough_review", "review"]
        .map(String::from)
        .to_vec();
    let out = revise(
        "r",
        &s,
        Change::Steps {
            by: "a".into(),
            source: ProposalSource::PlanBlock {
                ord: 4,
                attempt: 0,
                plan_block_seq: 1,
            },
            kind: ProposalKind::Change,
            reason: Some(ReviseReason::PaAdded),
            steps: vec![PlanStep {
                catalog: "build".into(),
                id: "fix".into(),
                ..PlanStep::default()
            }],
            touch: None,
            scoring: None,
        },
        &done,
        &hc,
        Some(4),
        0,
    )
    .unwrap();
    assert_eq!(
        def_ids(&out),
        [
            "build",
            "walkthrough_plan",
            "walkthrough_review",
            "review",
            "fix",
            "walkthrough_plan-floor",
            "walkthrough_review-floor"
        ]
    );
}

/// WT-C3 (codex review): holding a re-score never loses the waiting raise — a later lower but
/// destructive diff keeps the waiting score (90) and adds the destructive signal; obligations
/// accumulate; the fact published first is the fresh one only when its band rises.
#[test]
fn wt_c3_holding_a_rescore_keeps_the_highest_waiting_score() {
    let ob = |r: &str| crate::plan::HeldObligation {
        rule: r.into(),
        token: "step:test".into(),
    };
    let Change::Floor(mut waiting) = floor(90) else {
        unreachable!()
    };
    waiting.obligations = vec![ob("A")];
    let Change::Floor(mut fresh) = floor(20) else {
        unreachable!()
    };
    fresh.destructive = true;
    fresh.rescore_seq = 2;
    fresh.obligations = vec![ob("B")];
    let held = hold_rescore(Some(&waiting), fresh.clone(), true);
    assert_eq!((held.score, held.destructive), (90, true));
    assert_eq!(held.rescore_seq, 2, "the band rose: the fresh fact leads");
    assert_eq!(held.obligations, [ob("A"), ob("B")]);
    let held = hold_rescore(Some(&waiting), fresh.clone(), false);
    assert_eq!(held.rescore_seq, 1, "no band rise: the waiting fact stays");
    assert_eq!((held.score, held.destructive), (90, true));
    assert_eq!(hold_rescore(None, fresh.clone(), false), fresh);
}

// ── DES-TEAMING-002 rev 15 / DES-ASK-TEAM-CHAT-001 §4.3, §4.7 (ASK-K2b): crossing into work ─────

fn pa_block(steps: &str, touch: Option<&str>, scoring: Option<ScoreScope>) -> Change {
    let mut changes = changes_from_output(
        &format!(
            "PLAN+ {{\"steps\":{steps}{}}}",
            touch.map(|t| format!(",\"touch\":{t}")).unwrap_or_default()
        ),
        "a",
        1,
        0,
    );
    assert_eq!(changes.len(), 1);
    let mut c = changes.pop().unwrap();
    if let Change::Steps { scoring: s, .. } = &mut c {
        *s = scoring;
    }
    c
}

fn body<'a, T>(r: &'a Revised, pick: impl Fn(&'a TeamBody) -> Option<T>) -> Vec<T> {
    r.events.iter().filter_map(|e| pick(&e.body)).collect()
}

/// A PA `PLAN+` that adds the path's FIRST creator step to an accepted creator-less plan crosses
/// into work: its `touch` rides `plan.proposed`, it is scored as an intent score
/// (`path.scored{basis:"intent", score_source:"intent:<pid>"}` — 100 with no graph to read, X1's
/// fail-closed rule), it is HELD for approval in auto mode with reason `first_creator`, and the
/// held rev's touch source is `pa_scope`.
#[test]
fn a_first_creator_pa_change_is_scored_from_its_touch_and_held_as_first_creator() {
    let hc = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"understand","id":"answer-1"}]), 0, &hc);
    assert!(!s.accepted.as_ref().unwrap().steps.has_creator());
    let change = pa_block(
        r#"[{"catalog":"build","id":"build"}]"#,
        Some(r#"["src/retire.ts"]"#),
        Some(ScoreScope {
            repo_root: Some(std::env::temp_dir()),
            base_commit: None,
        }),
    );
    let r = revise("r", &s, change, &["answer-1".into()], &hc, Some(1), 0).unwrap();
    assert!(
        matches!(r.outcome, Outcome::Held { .. }),
        "crossing into work pauses in auto mode"
    );
    let proposed = body(&r, |b| match b {
        TeamBody::PlanProposed(p) => Some(p.clone()),
        _ => None,
    });
    assert_eq!(proposed.len(), 1);
    assert_eq!(proposed[0].kind, ProposalKind::Change);
    assert_eq!(proposed[0].touch, ["src/retire.ts"]);
    let scored = body(&r, |b| match b {
        TeamBody::PathScored(p) => Some(p.clone()),
        _ => None,
    });
    assert_eq!(
        scored.len(),
        1,
        "one intent score for the first creator step"
    );
    assert_eq!(scored[0].basis, crate::team::events::ScoreBasis::Intent);
    assert_eq!(
        scored[0].score_source,
        format!("intent:{}", proposed[0].proposal_id)
    );
    assert_eq!(scored[0].score, 100, "no graph to read: fail closed");
    let pending = r.state.pending.as_ref().expect("held");
    assert_eq!(pending.reason, "first_creator");
    assert_eq!(pending.touch_source, Some(TouchSource::PaScope));
    assert_eq!(
        pending.steps.touch.as_deref(),
        Some(&["src/retire.ts".to_string()][..])
    );
    assert!(
        pending.high_risk,
        "100 is high risk; the reason stays the more specific row"
    );
    assert_eq!(r.state.max_score, 100, "the ratchet records it");
}

/// The second creator addition (the accepted plan already has one) is an ordinary change: no
/// re-score, below high risk it proceeds in auto mode, and its touch is not read (`[]`).
#[test]
fn a_later_creator_addition_is_not_scored_and_proceeds_below_high_risk() {
    let hc = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"build"},{"catalog":"review"}]), 25, &hc);
    let change = pa_block(
        r#"[{"catalog":"produce","id":"docs"}]"#,
        Some(r#"["docs/x.md"]"#),
        Some(ScoreScope::default()),
    );
    let r = revise("r", &s, change, &["build".into()], &hc, Some(1), 0).unwrap();
    assert!(matches!(r.outcome, Outcome::Accepted { .. }));
    assert!(body(&r, |b| matches!(b, TeamBody::PathScored(_)).then_some(())).is_empty());
    let proposed = body(&r, |b| match b {
        TeamBody::PlanProposed(p) => Some(p.clone()),
        _ => None,
    });
    assert!(
        proposed[0].touch.is_empty(),
        "rev 14: a change that is not the first creator carries []"
    );
    let a = r.state.accepted.as_ref().unwrap();
    assert_eq!(
        a.touch_source,
        Some(TouchSource::User),
        "the base's source stands"
    );
    assert_eq!(
        a.touch,
        ["src/x.rs"],
        "the launch's declared touch, unchanged"
    );
}

/// A PA change that adds no creator step never carries a touch, even when its block spells one.
#[test]
fn a_read_only_pa_change_ignores_a_declared_touch() {
    let hc = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"understand","id":"answer-1"}]), 0, &hc);
    let change = pa_block(
        r#"[{"catalog":"understand","id":"answer-2"}]"#,
        Some(r#"["src/retire.ts"]"#),
        Some(ScoreScope::default()),
    );
    let r = revise("r", &s, change, &["answer-1".into()], &hc, Some(1), 0).unwrap();
    assert!(
        matches!(r.outcome, Outcome::Accepted { .. }),
        "read-only additions proceed"
    );
    let proposed = body(&r, |b| match b {
        TeamBody::PlanProposed(p) => Some(p.clone()),
        _ => None,
    });
    assert!(proposed[0].touch.is_empty());
    assert!(body(&r, |b| matches!(b, TeamBody::PathScored(_)).then_some(())).is_empty());
    assert_eq!(r.state.max_score, 0, "nothing was scored");
}

/// A human edit that adds the first creator step is still the human's own (rev 14): accepted as
/// the next rev with no score and no gate — the first-creator row is about the PA's `kind:"change"`.
#[test]
fn a_human_edit_adding_the_first_creator_step_stays_human_accepted() {
    let hc = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"understand","id":"answer-1"}]), 0, &hc);
    let change = Change::Steps {
        by: "human".into(),
        source: ProposalSource::Edit {
            request_id: "req-1".into(),
        },
        kind: ProposalKind::Edit,
        reason: None,
        steps: plan(json!({"steps":[{"catalog":"build","id":"build"}]})).steps,
        touch: None,
        scoring: None,
    };
    let r = revise("r", &s, change, &["answer-1".into()], &hc, Some(1), 0).unwrap();
    assert!(matches!(r.outcome, Outcome::Accepted { .. }));
    assert!(body(&r, |b| matches!(b, TeamBody::PathScored(_)).then_some(())).is_empty());
    assert_eq!(r.state.accepted.as_ref().unwrap().by, "human");
}

/// (§4.7 F11) On a repo-less path a first creator step that EXECUTES CODE has no worktree: the
/// proposal is refused on the record (`plan.refused`, "no repo bound") and the run keeps its rev.
#[test]
fn a_code_executing_first_creator_step_on_a_repo_less_path_is_refused() {
    let hc = HumanConfirm::None;
    let s = accepted(json!([{"catalog":"understand","id":"answer-1"}]), 0, &hc);
    let change = pa_block(
        r#"[{"catalog":"build","id":"build"}]"#,
        Some(r#"["src/retire.ts"]"#),
        Some(ScoreScope {
            repo_root: None,
            base_commit: None,
        }),
    );
    let r = revise("r", &s, change, &["answer-1".into()], &hc, Some(1), 0).unwrap();
    match &r.outcome {
        Outcome::Refused { reason } => assert_eq!(reason, "no repo bound"),
        _ => panic!("a code-executing step with no worktree must be refused"),
    }
    let refused = body(&r, |b| match b {
        TeamBody::PlanRefused(p) => Some(p.reason.clone()),
        _ => None,
    });
    assert_eq!(refused.len(), 1);
    assert!(refused[0].starts_with("no repo bound"), "{}", refused[0]);
    assert_eq!(r.state, s, "the run keeps its accepted rev");
    // `produce` (an artifact, no code) on the same path is scored and held, not refused.
    let docs = pa_block(
        r#"[{"catalog":"produce","id":"docs"}]"#,
        Some(r#"["docs/x.md"]"#),
        Some(ScoreScope::default()),
    );
    let r = revise("r", &s, docs, &["answer-1".into()], &hc, Some(1), 0).unwrap();
    assert!(matches!(r.outcome, Outcome::Held { .. }));
}

/// (§4.3 "a declined proposal leaves the accepted rev alone") The floor is computed on the
/// ACCEPTED plan: a creator-less plan has an empty floor whatever band the run has recorded, so
/// after a held first-creator proposal is not taken the accepted rev stands with nothing to add.
#[test]
fn a_creator_less_accepted_plan_has_an_empty_floor_at_any_recorded_band() {
    let hc = HumanConfirm::None;
    let answer = plan(json!({"steps":[{"catalog":"understand","id":"answer-1"}]}));
    for score in [0u8, 45, 100] {
        let filled = crate::plan::floor_fill(
            crate::catalog::catalog(),
            &answer,
            crate::plan::FloorInput {
                score,
                destructive: false,
                human_confirm: &hc,
                deliver: None,
                obligations: &[],
                ran: &[],
            },
        )
        .unwrap();
        assert_eq!(
            filled.steps.steps.len(),
            1,
            "band of {score}: nothing added"
        );
        assert!(!filled.high_risk, "never high risk without a creator step");
    }
}
