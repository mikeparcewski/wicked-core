//! core#630 round 3: `Core::propose_plan`'s synchronous answers, against a store fixture (no
//! engine): a spent request id is a duplicate whatever the run's state; an edit after the deliver
//! step started is refused up front; an edit is validated like a launch plan.

use super::*;
use crate::domain::{AgentSession, HumanConfirm, SessionStatus, UnitStatus, WorkUnit};
use crate::plan_gate::{AcceptedPlan, TeamPlanState};
use crate::scope::EntityMode;
use wicked_apps_core::{open_store, ToNode};

fn plan(v: serde_json::Value) -> crate::plan::PlanSteps {
    serde_json::from_value(v).unwrap()
}

/// Run `r` with an accepted rev 1 over `steps`; `units` are `(phase id, status, last_attempt)` in
/// order; the cursor is `cursor`.
fn fixture(
    store: &mut dyn GraphStore,
    status: SessionStatus,
    steps: serde_json::Value,
    units: &[(&str, UnitStatus, Option<u32>)],
    cursor: u32,
    edit_requests: &[&str],
) {
    let accepted = AcceptedPlan {
        rev: 1,
        by: "engine".into(),
        band: "0-19".into(),
        high_risk: false,
        auto: true,
        steps: plan(serde_json::json!({ "steps": steps })),
        floor_override: None,
        proposal_id: "p-fixture".into(),
    };
    let session = AgentSession {
        id: "r".into(),
        workflow_id: "wf-r".into(),
        problem: "p".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: vec!["claude".into()],
        status,
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        unit_ix: cursor as usize,
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
            accepted: Some(accepted),
            edit_requests: edit_requests.iter().map(|r| r.to_string()).collect(),
            ..TeamPlanState::default()
        }),
    };
    put_node(store, session.to_node()).unwrap();
    for (i, (phase, st, last)) in units.iter().enumerate() {
        let mut u = WorkUnit::pending(format!("r:{phase}"), "r", i as u32 + 1, *phase);
        u.status = *st;
        u.last_attempt = *last;
        u.assigned_cli = Some("claude".into());
        put_node(store, u.to_node()).unwrap();
    }
}

fn held(store: &dyn GraphStore) -> usize {
    crate::domain::get_session(store, "r")
        .unwrap()
        .unwrap()
        .team_plan
        .unwrap()
        .edits
        .len()
}

/// (round 3, LOW) A retried request id answers `duplicate: true` even after the run finished —
/// the spent id is consulted before the run's state.
#[test]
fn a_spent_request_id_is_a_duplicate_even_on_a_finished_run() {
    let mut store = open_store(Some(":memory:")).unwrap();
    fixture(
        &mut store,
        SessionStatus::Completed,
        serde_json::json!([{"catalog": "understand", "id": "understand", "added_by": "plan"}]),
        &[("understand", UnitStatus::Done, Some(0))],
        1,
        &["req-1"],
    );
    let add = plan(serde_json::json!({"steps": [{"catalog": "design"}]}));
    let p = propose_plan(&mut store, "r", add.clone(), "req-1").unwrap();
    assert!(p.duplicate);
    // A new id on the finished run is still refused.
    assert!(propose_plan(&mut store, "r", add, "req-2").is_err());
}

/// (round 3, MEDIUM) Once the deliver step has started (dispatched, or done), nothing can be
/// added after it: the edit is refused up front, never held and then silently dropped.
#[test]
fn an_edit_after_the_deliver_step_started_is_refused_up_front() {
    let mut store = open_store(Some(":memory:")).unwrap();
    fixture(
        &mut store,
        SessionStatus::Executing,
        serde_json::json!([
            {"catalog": "understand", "id": "understand", "added_by": "plan"},
            {"catalog": "deliver", "id": "deliver", "added_by": "plan",
             "executor": {"type": "tool", "cmd": ["true"]}}
        ]),
        &[
            ("understand", UnitStatus::Done, Some(0)),
            ("deliver", UnitStatus::Distributed, Some(0)),
        ],
        1,
        &[],
    );
    let e = propose_plan(
        &mut store,
        "r",
        plan(serde_json::json!({"steps": [{"catalog": "design"}]})),
        "req-d",
    )
    .unwrap_err();
    assert!(e.to_string().contains("deliver"), "{e}");
    assert_eq!(held(&store), 0);
}

/// (round 3, LOW) An edit is validated when it is proposed, as a launch plan is: an unknown
/// catalog id, a step the plan already has, or supplied provenance is refused synchronously
/// (nothing held, the request id not spent); a good edit is held.
#[test]
fn an_edit_is_validated_like_a_launch_plan() {
    let mut store = open_store(Some(":memory:")).unwrap();
    fixture(
        &mut store,
        SessionStatus::Executing,
        serde_json::json!([
            {"catalog": "understand", "id": "understand", "added_by": "plan"},
            {"catalog": "critique", "id": "critique", "added_by": "plan"}
        ]),
        &[
            ("understand", UnitStatus::Done, Some(0)),
            ("critique", UnitStatus::Distributed, None),
        ],
        1,
        &[],
    );
    for bad in [
        serde_json::json!({"steps": [{"catalog": "nope"}]}),
        serde_json::json!({"steps": [{"catalog": "critique"}]}),
        serde_json::json!({"steps": [{"catalog": "design", "added_by": "floor"}]}),
    ] {
        assert!(
            propose_plan(&mut store, "r", plan(bad.clone()), "req-v").is_err(),
            "{bad}"
        );
    }
    assert_eq!(held(&store), 0);
    let ok = propose_plan(
        &mut store,
        "r",
        plan(serde_json::json!({"steps": [{"catalog": "design"}]})),
        "req-v",
    )
    .unwrap();
    assert!(!ok.duplicate, "the refused calls did not spend the id");
    assert_eq!(held(&store), 1);
}

/// Rewrite the fixture run's plan state.
fn with_plan_state(store: &mut dyn GraphStore, f: impl FnOnce(&mut TeamPlanState)) {
    let mut s = crate::domain::get_session(&*store, "r").unwrap().unwrap();
    f(s.team_plan.as_mut().unwrap());
    put_node(store, s.to_node()).unwrap();
}

/// (round 4) The answer shows what the edit does to the run's risk (the operator decision stands:
/// the human's own edit opens no gate, so this is where they see it): the dry run's `band`,
/// `high_risk` and the floor phases it adds. A plan with no creator at a ratcheted 100 has an
/// empty floor; adding `produce` makes it owe the 70-100 floor.
#[test]
fn the_answer_carries_the_band_high_risk_and_floor_additions_of_the_edit() {
    let mut store = open_store(Some(":memory:")).unwrap();
    fixture(
        &mut store,
        SessionStatus::Executing,
        serde_json::json!([
            {"catalog": "understand", "id": "understand", "added_by": "plan"},
            {"catalog": "critique", "id": "critique", "added_by": "plan"}
        ]),
        &[
            ("understand", UnitStatus::Done, Some(0)),
            ("critique", UnitStatus::Distributed, None),
        ],
        1,
        &[],
    );
    with_plan_state(&mut store, |tp| tp.max_score = 100);
    let p = propose_plan(
        &mut store,
        "r",
        plan(serde_json::json!({"steps": [{"catalog": "produce"}]})),
        "req-risk",
    )
    .unwrap();
    let v = serde_json::to_value(&p).unwrap();
    let mut keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "band",
            "duplicate",
            "floor_added",
            "high_risk",
            "proposal_id"
        ]
    );
    assert_eq!(v["band"], "70-100");
    assert_eq!(v["high_risk"], true);
    let mut added: Vec<&str> = v["floor_added"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a.as_str().unwrap())
        .collect();
    added.sort_unstable();
    assert_eq!(
        added,
        ["architecture", "design", "security_review", "test_plan"]
    );
    // A duplicate re-decides nothing: no band, no risk, no additions.
    let again = propose_plan(
        &mut store,
        "r",
        plan(serde_json::json!({"steps": [{"catalog": "produce"}]})),
        "req-risk",
    )
    .unwrap();
    let a = serde_json::to_value(&again).unwrap();
    assert_eq!(
        (
            a["duplicate"].clone(),
            a["band"].clone(),
            a["high_risk"].clone()
        ),
        (
            true.into(),
            serde_json::Value::Null,
            serde_json::Value::Null
        )
    );
}

/// (round 4) While a plan is held for approval, a mid-run edit is refused (the edit belongs at
/// the gate, where it is the answer): nothing is held and the request id is not spent.
#[test]
fn an_edit_while_a_plan_awaits_approval_is_refused() {
    let mut store = open_store(Some(":memory:")).unwrap();
    fixture(
        &mut store,
        SessionStatus::AwaitingHuman,
        serde_json::json!([
            {"catalog": "understand", "id": "understand", "added_by": "plan"},
            {"catalog": "critique", "id": "critique", "added_by": "plan"}
        ]),
        &[
            ("understand", UnitStatus::Done, Some(0)),
            ("critique", UnitStatus::Distributed, None),
        ],
        1,
        &[],
    );
    with_plan_state(&mut store, |tp| {
        tp.rev = 2;
        tp.pending = Some(crate::plan_gate::PendingPlan {
            rev: 2,
            proposal_id: "p-held".into(),
            reviewing_ord: Some(1),
            steps: tp.accepted.as_ref().unwrap().steps.clone(),
            band: "0-19".into(),
            high_risk: false,
            floor_override: None,
            reason: "manual_mode".into(),
            floor_added: Vec::new(),
            gate_id: Some("g-r-1".into()),
            refusal: None,
        });
    });
    let e = propose_plan(
        &mut store,
        "r",
        plan(serde_json::json!({"steps": [{"catalog": "design"}]})),
        "req-g",
    )
    .unwrap_err();
    assert!(e.to_string().contains("awaiting approval"), "{e}");
    assert_eq!(held(&store), 0);
    let tp = crate::domain::get_session(&store, "r")
        .unwrap()
        .unwrap()
        .team_plan
        .unwrap();
    assert!(tp.edit_requests.is_empty(), "the request id is not spent");
}
