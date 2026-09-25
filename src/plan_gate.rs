//! Seam T3 (DES-TEAMING-002 §8.4-§8.6): the plan approval gate — RED STUB.

use serde::{Deserialize, Serialize};

use crate::domain::HumanConfirm;
use crate::plan::{FloorOverride, PlanSteps};
use crate::review_scale::{Assessment, Graph};
use crate::team::events::{ApprovalReason, ProposalKind, ProposalSource, TeamEvent};
use crate::workflow::WorkflowDef;

/// The `gate_kind` token of a plan approval pause (`AwaitingHuman.gate_kind`, the durable
/// interaction row, `gate.opened.kind`).
pub(crate) const GATE_KIND: &str = "plan_approval";

/// A run's plan state (persisted on `AgentSession.team_plan`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TeamPlanState {
    pub rev: u32,
    pub accepted_rev: u32,
    pub accepted_high_risk: bool,
    pub approved_high_risk: bool,
    pub max_score: u8,
    pub destructive: bool,
    pub preset: Option<String>,
    pub pending: Option<PendingPlan>,
    pub roster: Vec<serde_json::Value>,
}

/// A composed plan awaiting approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPlan {
    pub rev: u32,
    pub proposal_id: String,
    pub reviewing_ord: Option<u32>,
    pub steps: PlanSteps,
    pub band: String,
    pub high_risk: bool,
    pub floor_override: Option<FloorOverride>,
    pub reason: String,
    pub floor_added: Vec<String>,
    pub gate_id: Option<String>,
    pub refusal: Option<String>,
}

/// Which row of the approval matrix a plan event is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanEvent {
    Initial,
    Revision {
        previous_high_risk: bool,
        approved_high_risk: bool,
    },
}

pub(crate) fn is_auto(_hc: &HumanConfirm) -> bool {
    false
}

pub(crate) fn approval(
    _auto: bool,
    _high_risk: bool,
    _has_override: bool,
    _event: PlanEvent,
) -> Result<Option<ApprovalReason>, &'static str> {
    Ok(None)
}

pub(crate) fn with_default_ids(plan: &PlanSteps) -> PlanSteps {
    plan.clone()
}

pub(crate) struct Scored {
    pub assessment: Assessment,
    pub destructive: bool,
}

pub(crate) fn intent_score(plan: &PlanSteps, graph: Graph<'_>) -> Scored {
    let touch: Vec<&str> = plan.touch.iter().flatten().map(String::as_str).collect();
    Scored {
        assessment: crate::review_scale::assess_intent(false, Some(&touch), graph, None),
        destructive: false,
    }
}

pub(crate) struct Proposal {
    pub by: String,
    pub source: ProposalSource,
    pub kind: ProposalKind,
    pub preset: Option<String>,
    pub plan: PlanSteps,
    pub reviewing_ord: Option<u32>,
    pub approved_by_human: bool,
}

pub(crate) enum Verdict {
    Accepted { rev: u32, def: WorkflowDef },
    Held { rev: u32, def: WorkflowDef },
    Refused { reason: String },
}

pub(crate) struct Decided {
    pub state: TeamPlanState,
    pub events: Vec<TeamEvent>,
    pub verdict: Verdict,
}

pub(crate) fn decide(
    _run_id: &str,
    _proposal: Proposal,
    prior: &TeamPlanState,
    _human_confirm: &HumanConfirm,
    _scored: &Scored,
    _now: i64,
) -> anyhow::Result<Decided> {
    Ok(Decided {
        state: prior.clone(),
        events: Vec::new(),
        verdict: Verdict::Refused {
            reason: "unbuilt".into(),
        },
    })
}

/// The ONE call site every T3 fact goes through (stub).
pub(crate) fn publish(_sink: &mut crate::event_log::EventSink, _ev: &TeamEvent) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_scale::Graph;
    use crate::team::events::{TeamBody, PATH_SCORED, PLAN_ACCEPTED, PLAN_PROPOSED};
    use serde_json::json;

    fn plan(v: serde_json::Value) -> PlanSteps {
        serde_json::from_value(v).unwrap()
    }

    /// DES-TEAMING-002 §8.6's approval matrix, row by row (fixed expectations from the table).
    #[test]
    fn the_approval_matrix_row_by_row() {
        use ApprovalReason::*;
        let init = PlanEvent::Initial;
        // Initial plan: manual → approval; auto not high risk → proceeds; auto high risk → approval.
        assert_eq!(approval(false, false, false, init), Ok(Some(ManualMode)));
        assert_eq!(approval(false, true, false, init), Ok(Some(ManualMode)));
        assert_eq!(approval(true, false, false, init), Ok(None));
        assert_eq!(approval(true, true, false, init), Ok(Some(HighRisk)));
        // Revision INTO high risk: approval in both modes.
        let into = PlanEvent::Revision {
            previous_high_risk: false,
            approved_high_risk: false,
        };
        assert_eq!(approval(false, true, false, into), Ok(Some(ManualMode)));
        assert_eq!(approval(true, true, false, into), Ok(Some(IntoHighRisk)));
        // Revision that STAYS high risk after an approved high-risk rev: manual approval, auto proceeds.
        let stays = PlanEvent::Revision {
            previous_high_risk: true,
            approved_high_risk: true,
        };
        assert_eq!(approval(false, true, false, stays), Ok(Some(ManualMode)));
        assert_eq!(approval(true, true, false, stays), Ok(None));
        // …but high risk that was never approved still needs it (positive evidence only).
        let never = PlanEvent::Revision {
            previous_high_risk: true,
            approved_high_risk: false,
        };
        assert_eq!(approval(true, true, false, never), Ok(Some(HighRisk)));
        // Any other revision: manual approval, auto proceeds.
        let other = PlanEvent::Revision {
            previous_high_risk: false,
            approved_high_risk: false,
        };
        assert_eq!(approval(false, false, false, other), Ok(Some(ManualMode)));
        assert_eq!(approval(true, false, false, other), Ok(None));
        // A floor override: approval in manual, refused in auto (whatever the risk).
        assert_eq!(approval(false, false, true, init), Ok(Some(Override)));
        assert_eq!(
            approval(true, false, true, init),
            Err("override in auto mode")
        );
        assert_eq!(
            approval(true, true, true, init),
            Err("override in auto mode")
        );
    }

    /// Auto mode is read from the run's autonomy and nothing else (§8.6).
    #[test]
    fn auto_mode_is_human_confirm_none_only() {
        assert!(is_auto(&HumanConfirm::None));
        assert!(!is_auto(&HumanConfirm::All));
        assert!(!is_auto(&HumanConfirm::Before(1)));
    }

    /// A step with no `id` takes its catalog id; a repeat takes `<catalog>-2`, `-3`, …; an
    /// authored id is never changed.
    #[test]
    fn omitted_step_ids_default_to_the_catalog_id() {
        let p = plan(json!({"steps": [
            {"catalog": "build"}, {"catalog": "review"}, {"catalog": "review"},
            {"catalog": "test", "id": "prove"}
        ]}));
        let ids: Vec<String> = with_default_ids(&p)
            .steps
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, ["build", "review", "review-2", "prove"]);
    }

    fn scored_for(p: &PlanSteps) -> Scored {
        intent_score(p, Graph::Unavailable("no repo".into()))
    }

    fn body_types(d: &Decided) -> Vec<&'static str> {
        d.events.iter().map(TeamEvent::event_type).collect()
    }

    /// The matrix binds a PA-composed plan exactly as it binds a user plan (T3: "on a PA plan and
    /// again on a user-composed plan"): the author changes `by` and the proposal id, nothing else.
    #[test]
    fn a_pa_plan_and_a_user_plan_meet_the_same_matrix() {
        let authors = [
            (
                "claude#1",
                ProposalSource::Understand { ord: 1, attempt: 0 },
                Some(1),
            ),
            (
                "human",
                ProposalSource::Launch {
                    session_id: "r1".into(),
                },
                None,
            ),
        ];
        for (by, source, reviewing) in authors {
            let prop = |p: PlanSteps| Proposal {
                by: by.into(),
                source: source.clone(),
                kind: ProposalKind::Initial,
                preset: None,
                plan: p,
                reviewing_ord: reviewing,
                approved_by_human: false,
            };
            let read_only = plan(json!({"steps": [{"catalog": "understand", "id": "u"}]}));
            let creator = plan(json!({"steps": [{"catalog": "build", "id": "b"}]}));
            let prior = TeamPlanState::default();

            // (b) auto, band < 70, no destructive: released by the engine.
            let d = decide(
                "r1",
                prop(read_only.clone()),
                &prior,
                &HumanConfirm::None,
                &scored_for(&read_only),
                1,
            )
            .unwrap();
            assert!(
                matches!(d.verdict, Verdict::Accepted { rev: 1, .. }),
                "{by}"
            );
            assert_eq!(
                body_types(&d),
                [PLAN_PROPOSED, PATH_SCORED, PLAN_ACCEPTED],
                "{by}"
            );
            let TeamBody::PlanAccepted(a) = &d.events[2].body else {
                panic!()
            };
            assert_eq!(
                (d.events[2].env.by.as_str(), a.plan_rev, a.high_risk),
                ("engine", 1, false)
            );
            assert_eq!(a.workflow_id, "r1:plan-1");
            let TeamBody::PlanProposed(p) = &d.events[0].body else {
                panic!()
            };
            assert_eq!(d.events[0].env.by, by);
            assert_eq!(
                p.proposal_id,
                crate::team::events::mint_proposal_id("r1", by, &source)
            );
            assert_eq!((d.state.accepted_rev, d.state.pending.is_none()), (1, true));

            // (c) auto, high risk (a creator plan with no declared scope scores 100): held.
            let d = decide(
                "r1",
                prop(creator.clone()),
                &prior,
                &HumanConfirm::None,
                &scored_for(&creator),
                1,
            )
            .unwrap();
            assert!(matches!(d.verdict, Verdict::Held { rev: 1, .. }), "{by}");
            assert_eq!(body_types(&d), [PLAN_PROPOSED, PATH_SCORED], "{by}");
            let held = d.state.pending.as_ref().expect("pending");
            assert_eq!(
                (held.reason.as_str(), held.high_risk, held.band.as_str()),
                ("high_risk", true, "70-100")
            );
            assert_eq!(held.reviewing_ord, reviewing);
            assert_eq!(held.gate_id, None, "the gate opens at the step boundary");
            assert_eq!(d.state.accepted_rev, 0);

            // (a) manual: held even when not high risk.
            let d = decide(
                "r1",
                prop(read_only.clone()),
                &prior,
                &HumanConfirm::Before(1),
                &scored_for(&read_only),
                1,
            )
            .unwrap();
            assert!(matches!(d.verdict, Verdict::Held { .. }), "{by}");
            assert_eq!(d.state.pending.as_ref().unwrap().reason, "manual_mode");
        }
    }

    /// A supplied value never skips a pause: the score, band and `high_risk` come from the
    /// computation, and a creator plan's destructive touch is high risk in any band.
    #[test]
    fn a_destructive_touch_is_held_in_auto_mode() {
        let p = plan(
            json!({"steps": [{"catalog": "build", "id": "b"}], "touch": ["db/migrations/001.sql"]}),
        );
        let mut s = scored_for(&p);
        // Even a (hypothetically) low score is high risk when the touch set is destructive.
        s.assessment.score = 10;
        s.assessment.deterministic = 10;
        let prop = Proposal {
            by: "human".into(),
            source: ProposalSource::Launch {
                session_id: "r".into(),
            },
            kind: ProposalKind::Initial,
            preset: None,
            plan: p,
            reviewing_ord: None,
            approved_by_human: false,
        };
        let d = decide(
            "r",
            prop,
            &TeamPlanState::default(),
            &HumanConfirm::None,
            &s,
            1,
        )
        .unwrap();
        assert!(matches!(d.verdict, Verdict::Held { .. }));
        assert!(d.state.pending.unwrap().high_risk);
    }

    /// `publish` hands the sink exactly the bus row `TeamEvent::bus_emit` builds.
    #[test]
    fn publish_hands_over_the_bus_row() {
        let p = plan(json!({"steps": [{"catalog": "understand", "id": "u"}]}));
        let d = decide(
            "r",
            Proposal {
                by: "human".into(),
                source: ProposalSource::Launch {
                    session_id: "r".into(),
                },
                kind: ProposalKind::Initial,
                preset: None,
                plan: p.clone(),
                reviewing_ord: None,
                approved_by_human: false,
            },
            &TeamPlanState::default(),
            &HumanConfirm::None,
            &scored_for(&p),
            1,
        )
        .unwrap();
        let mut sink = crate::event_log::EventSink::default();
        let (tx, rx) = std::sync::mpsc::channel();
        sink.push(tx);
        publish(&mut sink, &d.events[0]);
        let row = d.events[0].bus_emit().unwrap();
        match rx.try_recv().unwrap() {
            crate::CoreEvent::TeamFact {
                session,
                event_type,
                key,
                payload,
            } => {
                assert_eq!(session, "r");
                assert_eq!(event_type, row.event_type);
                assert_eq!(Some(key), row.idempotency_key);
                assert_eq!(payload, row.payload);
            }
            other => panic!("{other:?}"),
        }
    }
}
