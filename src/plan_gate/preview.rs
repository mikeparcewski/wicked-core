//! (DES-TEAMING-002 T8 (e), `Core.previewPlan`) What a launch would compute for a plan, with
//! nothing persisted and nothing published: the SAME functions the launch runs — [`super::precheck`],
//! [`super::intent_score_for_run`] and [`super::decide`] over a fresh plan state — read back as
//! the intent score, the floor fill ([`crate::plan::FloorFilled`], field for field) and whether the
//! `plan_approval` gate would pause. A preview has no worktree, so it scores as a launch with no
//! repo does (a behavioural touch set fails closed at 100; a docs-only one scores 0), and it has
//! no deliver step (the floor carries no `deliver`).

use serde::Serialize;

use crate::domain::HumanConfirm;
use crate::plan::{FloorOverride, PlanStep, PlanSteps};
use crate::workflow::WorkflowDef;

/// The run id a preview's decision is made under (its composed def is `preview:plan-1`).
const PREVIEW_RUN: &str = "preview";

/// A plan preview (`POST /api/v1/plans/preview`). The floor-fill fields are
/// [`crate::plan::FloorFilled`]'s, with `steps` as the step list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PlanPreview {
    /// The intent score (§8.2): the final score, its deterministic part, one reason per
    /// contribution, and the destructive signal floor fill reads.
    pub score: u8,
    pub deterministic: u8,
    pub reasons: Vec<String>,
    pub destructive: bool,
    /// The floor band the score lands in (`"40-69"`) and §8.5's high-risk rule, computed.
    pub band: String,
    pub high_risk: bool,
    /// The floor phase types the plan owes, in order (`FloorFilled.floor`); the steps the floor
    /// ADDED are the `steps` with `added_by: "floor"`.
    pub floor: Vec<String>,
    /// The floor override as recorded (manual mode only).
    pub floor_override: Option<FloorOverride>,
    /// The floor-filled steps; each carries `added_by`, and `floor_reason` when the floor added it.
    pub steps: Vec<PlanStep>,
    /// `compose` of `steps`: the def the run would plan from.
    pub def: WorkflowDef,
    /// The approval matrix (§8.6): `true` when the launch would pause at a `plan_approval` gate
    /// before its first unit, with the reason token (`manual_mode`, `high_risk`, `override`).
    pub pauses: bool,
    pub pause_reason: Option<String>,
}

/// Preview `plan` as a launch with `human_confirm` would decide it. `Err` carries the refusal the
/// launch would return (compose, provenance, the override in auto mode, …).
pub fn preview_plan(plan: &PlanSteps, human_confirm: &HumanConfirm) -> anyhow::Result<PlanPreview> {
    // The launch's synchronous checks, then its decision — the same calls, in the same order.
    if let Err(r) = super::precheck(plan, None, human_confirm) {
        anyhow::bail!("the plan is refused: {}", r.reason);
    }
    let scored = super::intent_score_for_run(plan, None, None);
    let decided = super::decide(
        PREVIEW_RUN,
        super::Proposal {
            by: "human".into(),
            source: crate::team::events::ProposalSource::Launch {
                session_id: PREVIEW_RUN.into(),
            },
            kind: crate::team::events::ProposalKind::Initial,
            preset: None,
            plan: plan.clone(),
            reviewing_ord: None,
            approved_by_human: false,
        },
        &super::TeamPlanState::default(),
        human_confirm,
        &scored,
        0,
    )?;
    if let super::Verdict::Refused { reason } = decided.verdict {
        anyhow::bail!("the plan is refused: {reason}");
    }
    let filled = decided
        .filled
        .ok_or_else(|| anyhow::anyhow!("the plan decision carried no floor fill"))?;
    let pending = decided.state.pending;
    let a = scored.assessment;
    Ok(PlanPreview {
        score: a.score,
        deterministic: a.deterministic,
        reasons: a.reasons,
        destructive: scored.destructive,
        band: filled.band,
        high_risk: filled.high_risk,
        floor: filled.floor,
        floor_override: filled.floor_override,
        steps: filled.steps.steps,
        def: filled.def,
        pauses: pending.is_some(),
        pause_reason: pending.map(|p| p.reason),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn plan(v: Value) -> PlanSteps {
        serde_json::from_value(v).unwrap()
    }

    fn preview(v: Value, hc: HumanConfirm) -> Value {
        serde_json::to_value(preview_plan(&plan(v), &hc).expect("a preview")).unwrap()
    }

    /// The exact key set, and a docs-only non-code plan in auto mode: score 0, band 0-19, no pause.
    #[test]
    fn a_docs_only_plan_in_auto_mode_proceeds_and_the_shape_is_pinned() {
        let p = preview(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}], "touch": ["README.md"]}),
            HumanConfirm::None,
        );
        let keys: Vec<&str> = p.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "band",
                "def",
                "destructive",
                "deterministic",
                "floor",
                "floor_override",
                "high_risk",
                "pause_reason",
                "pauses",
                "reasons",
                "score",
                "steps"
            ]
        );
        assert_eq!(p["score"], 0);
        assert_eq!(p["band"], "0-19");
        assert_eq!(p["high_risk"], false);
        assert_eq!(p["pauses"], false);
        assert_eq!(p["pause_reason"], Value::Null);
        assert_eq!(p["def"]["id"], "preview:plan-1");
        for s in p["steps"].as_array().unwrap() {
            assert!(
                s["added_by"].is_string(),
                "every step carries added_by: {s}"
            );
        }
        let ids: Vec<&str> = p["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids[0], "produce", "{ids:?}");
    }

    /// Manual mode pauses every initial plan (§8.6 row 1).
    #[test]
    fn manual_mode_pauses_for_approval() {
        let p = preview(
            json!({"steps": [{"catalog": "produce"}, {"catalog": "critique"}], "touch": ["README.md"]}),
            HumanConfirm::All,
        );
        assert_eq!(p["pauses"], true);
        assert_eq!(p["pause_reason"], "manual_mode");
    }

    /// A creator plan with no declared scope fails closed at 100: high risk, the top band's floor
    /// added (each floor step with its reason), and a pause in auto mode too.
    #[test]
    fn an_undeclared_creator_plan_is_high_risk_floor_filled_and_pauses_in_auto_mode() {
        let p = preview(json!({"steps": [{"catalog": "build"}]}), HumanConfirm::None);
        assert_eq!(p["score"], 100);
        assert_eq!(p["band"], "70-100");
        assert_eq!(p["high_risk"], true);
        assert_eq!(p["pauses"], true);
        assert_eq!(p["pause_reason"], "high_risk");
        assert!(!p["reasons"].as_array().unwrap().is_empty());
        let floor: Vec<&str> = p["floor"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap())
            .collect();
        assert!(
            floor.contains(&"build") && floor.contains(&"review"),
            "{floor:?}"
        );
        let added: Vec<&Value> = p["steps"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|s| s["added_by"] == "floor")
            .collect();
        assert!(!added.is_empty());
        assert!(added.iter().all(|s| s["floor_reason"].is_string()));
        // The authored step stays the plan's.
        let build = p["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["catalog"] == "build")
            .unwrap();
        assert_eq!(build["added_by"], "plan");
    }

    /// The launch's refusals are the preview's: an override in auto mode, an unknown catalog id,
    /// supplied provenance.
    #[test]
    fn the_launch_refusals_are_the_preview_refusals() {
        let auto = HumanConfirm::None;
        let e = preview_plan(
            &plan(json!({"steps": [{"catalog": "build"}], "override": {"remove": ["review"], "reason": "x"}})),
            &auto,
        )
        .unwrap_err();
        assert!(e.to_string().contains("override in auto mode"), "{e}");
        assert!(preview_plan(&plan(json!({"steps": [{"catalog": "nope"}]})), &auto).is_err());
        assert!(preview_plan(
            &plan(json!({"steps": [{"catalog": "build", "added_by": "floor"}]})),
            &auto
        )
        .is_err());
        // In manual mode the override is recorded and the plan pauses for it.
        let p = preview(
            json!({"steps": [{"catalog": "build"}],
                   "override": {"remove": ["design"], "reason": "x"}}),
            HumanConfirm::All,
        );
        assert_eq!(p["pause_reason"], "override");
        assert_eq!(
            p["floor_override"],
            json!({"remove": ["design"], "reason": "x"})
        );
        assert!(!p["steps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["catalog"] == "design"));
    }
}
