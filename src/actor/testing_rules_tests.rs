//! WT-C3 (DES-walkthrough-proof §4.12) on the actor's own seam, against a store fixture: the
//! supervisor's diff re-score re-derives the testing-rule context from the settled diff, and a
//! held rule it newly fires is held for the next step boundary (the same `floor_raised` path),
//! for a run in the rule's project only.

use super::*;
use crate::domain::{AgentSession, HumanConfirm, SessionStatus};
use crate::plan_gate::{AcceptedPlan, TeamPlanState};
use crate::scope::EntityMode;
use wicked_apps_core::{open_store, ToNode};

fn session(run: &str) -> AgentSession {
    let steps: crate::plan::PlanSteps = serde_json::from_value(serde_json::json!({"steps": [
        {"catalog": "build", "id": "build", "added_by": "plan"},
        {"catalog": "review", "id": "review", "added_by": "floor"}]}))
    .unwrap();
    AgentSession {
        intent_amendments: Vec::new(),
        id: run.into(),
        workflow_id: format!("{run}:plan-1"),
        problem: "p".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: vec!["claude".into()],
        status: SessionStatus::Executing,
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        unit_ix: 0,
        attempt: 0,
        workdir: None,
        repo_ref: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        project_id: None,
        archived_at: None,
        archive_note: None,
        verified_tree: None,
        run_branch: None,
        base_commit: None,
        finished_at: None,
        benched_seats: Vec::new(),
        team: None,
        team_plan: Some(TeamPlanState {
            rev: 1,
            accepted_rev: 1,
            max_score: 25,
            accepted: Some(AcceptedPlan {
                rev: 1,
                by: "engine".into(),
                band: "20-39".into(),
                high_risk: false,
                auto: true,
                steps,
                floor_override: None,
                proposal_id: "p-fixture".into(),
                touch: Vec::new(),
                touch_truncated: false,
                touch_source: None,
                rules: Vec::new(),
            }),
            ..TeamPlanState::default()
        }),
        exclude_seats: Vec::new(),
        evidence_root: None,
        assurance: Default::default(),
    }
}

fn rescored(store: &dyn GraphStore, run: &str) -> Option<crate::plan_gate::DiffRescore> {
    crate::domain::get_session(store, run)
        .unwrap()
        .unwrap()
        .team_plan
        .unwrap()
        .rescored
}

/// A docs-only diff (score 0: no band change) that a project-scoped held rule matches by path is
/// held for the boundary carrying the rule's obligation — once, however often the diff re-scores
/// — and a run filed in another project holds nothing.
#[test]
fn a_rescore_that_newly_fires_a_held_rule_is_held_for_the_boundary() {
    let _ = wicked_apps_core::emit::hermetic_test_spool();
    let mut store = open_store(Some(":memory:")).unwrap();
    let pay = crate::project::create_project(&mut store, "Payments", None, 1).unwrap();
    let other = crate::project::create_project(&mut store, "Other", None, 2).unwrap();
    for (project, run) in [(&pay.id, "r-pay"), (&other.id, "r-other")] {
        put_node(&mut store, session(run).to_node()).unwrap();
        crate::project::attach_member(
            &mut store,
            crate::project::MemberSpec {
                project_id: project.clone(),
                member_kind: crate::project::MEMBER_KIND_RUN.to_string(),
                member_ref: run.to_string(),
                meta: None,
                attached_by: "test".into(),
            },
            3,
        )
        .unwrap();
    }
    let rule: wicked_governance::ConformanceRule = serde_json::from_value(serde_json::json!({
        "id": "TST-1003", "rule_type": "policy", "statement": "Payments paths get a security review.",
        "severity": "warn", "confidence": 0.9, "steering_type": "testing",
        "applies_to": ["plan.compose"], "targets": {"project": pay.id},
        "effect": "allow_with_conditions", "obligations": ["step:security_review"],
        "trigger": {"contains": "\"paths\":\\[[^\\]]*\"docs/payments/"}}))
    .unwrap();
    wicked_governance::register_rule(&mut store, &rule).unwrap();
    // An advisory (recall-only) rule every project reads.
    let advisory: wicked_governance::ConformanceRule = serde_json::from_value(serde_json::json!({
        "id": "TST-1001", "rule_type": "policy", "statement": "Docs-only changes get the repo's checks only.",
        "severity": "info", "confidence": 0.9, "steering_type": "testing",
        "applies_to": ["plan.compose"], "trigger": {"contains": "\"kinds\":\\[\"docs\"\\]"}}))
    .unwrap();
    wicked_governance::register_rule(&mut store, &advisory).unwrap();
    let paths = vec!["docs/payments/refunds.md".to_string()];
    let tree = "0123456789abcdef0123456789abcdef01234567";
    super::on_rescored(&mut store, "r-pay", 1, 0, 1, tree, &paths).unwrap();
    let r = rescored(&store, "r-pay").expect("the newly fired rule is held");
    assert_eq!(r.score, 0, "a docs-only diff: no band change");
    assert_eq!(
        r.obligations,
        [crate::plan::HeldObligation {
            rule: "TST-1003".into(),
            token: "step:security_review".into(),
        }]
    );
    // A second re-score of the same diff adds nothing.
    super::on_rescored(&mut store, "r-pay", 1, 0, 2, tree, &paths).unwrap();
    assert_eq!(rescored(&store, "r-pay").unwrap().obligations.len(), 1);
    assert_eq!(rescored(&store, "r-pay").unwrap().recalled, ["TST-1001"]);
    // Another project's run: the held rule is not its, and a docs diff raises no band — but the
    // advisory rule it newly applied is ratcheted onto its record (Copilot review).
    super::on_rescored(&mut store, "r-other", 1, 0, 1, tree, &paths).unwrap();
    assert!(rescored(&store, "r-other").is_none());
    let tp = crate::domain::get_session(&store, "r-other")
        .unwrap()
        .unwrap()
        .team_plan
        .unwrap();
    assert_eq!(tp.recalled, ["TST-1001"]);
}

/// (Copilot review) A held re-score carrying an obligation outside the vocabulary (a `deny` row
/// written around the write-time check) fails the run at the boundary, naming the rule — the
/// raise is never dropped while the run goes on under its old floor.
#[test]
fn a_held_rescore_with_an_unholdable_rule_fails_the_boundary() {
    let _ = wicked_apps_core::emit::hermetic_test_spool();
    let mut store = open_store(Some(":memory:")).unwrap();
    let mut s = session("r-bad");
    super::refuse_unholdable_rules(&store, "r-bad").unwrap();
    let fact = crate::plan_gate::path_scored_diff(
        "r-bad",
        1,
        0,
        1,
        Some("t"),
        &crate::review_scale::assess_intent(
            true,
            Some(&["README.md"]),
            crate::review_scale::Graph::Unavailable("none".into()),
            None,
        ),
        0,
    )
    .unwrap();
    s.team_plan.as_mut().unwrap().rescored = Some(crate::plan_gate::DiffRescore {
        ord: 1,
        attempt: 0,
        rescore_seq: 1,
        score: 0,
        destructive: false,
        fact: crate::plan_gate::queued_facts(&[fact])
            .unwrap()
            .pop()
            .unwrap(),
        obligations: vec![crate::plan::HeldObligation {
            rule: "TST-DENY".into(),
            token: "effect:deny".into(),
        }],
        recalled: Vec::new(),
    });
    put_node(&mut store, s.to_node()).unwrap();
    let err = super::refuse_unholdable_rules(&store, "r-bad")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("TST-DENY") && err.contains("unknown_obligation"),
        "{err}"
    );
}

/// (core#846) A held re-score that newly fires a `step:security_review` rule on a run with no
/// code-executing step fails the run at the boundary, naming the rule — the revision would be
/// refused (`security_review_on_non_code_plan`) and the raise dropped while the run went on
/// without the review. On a code run the same rule is held as before.
#[test]
fn a_held_rescore_that_needs_a_security_review_on_a_non_code_run_fails_the_boundary() {
    let _ = wicked_apps_core::emit::hermetic_test_spool();
    let mut store = open_store(Some(":memory:")).unwrap();
    let fact = crate::plan_gate::path_scored_diff(
        "r-nc",
        1,
        0,
        1,
        Some("t"),
        &crate::review_scale::assess_intent(
            true,
            Some(&["docs/payments/refunds.md"]),
            crate::review_scale::Graph::Unavailable("none".into()),
            None,
        ),
        0,
    )
    .unwrap();
    let held = |run: &str, steps: serde_json::Value| {
        let mut s = session(run);
        let tp = s.team_plan.as_mut().unwrap();
        tp.accepted.as_mut().unwrap().steps = serde_json::from_value(steps).unwrap();
        tp.rescored = Some(crate::plan_gate::DiffRescore {
            ord: 1,
            attempt: 0,
            rescore_seq: 1,
            score: 0,
            destructive: false,
            fact: crate::plan_gate::queued_facts(std::slice::from_ref(&fact))
                .unwrap()
                .pop()
                .unwrap(),
            obligations: vec![crate::plan::HeldObligation {
                rule: "TST-1003".into(),
                token: "step:security_review".into(),
            }],
            recalled: Vec::new(),
        });
        s
    };
    put_node(
        &mut store,
        held(
            "r-nc",
            serde_json::json!({"steps": [
                {"catalog": "produce", "id": "produce", "added_by": "plan"},
                {"catalog": "critique", "id": "critique", "added_by": "floor"}]}),
        )
        .to_node(),
    )
    .unwrap();
    let err = super::refuse_unholdable_rules(&store, "r-nc")
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("TST-1003") && err.contains("security_review_on_non_code_plan"),
        "{err}"
    );
    put_node(
        &mut store,
        held(
            "r-code",
            serde_json::json!({"steps": [
                {"catalog": "build", "id": "build", "added_by": "plan"},
                {"catalog": "review", "id": "review", "added_by": "floor"}]}),
        )
        .to_node(),
    )
    .unwrap();
    super::refuse_unholdable_rules(&store, "r-code").unwrap();
}
