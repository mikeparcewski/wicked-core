//! (DES-TEAMING-002 T8 (e), `Core.previewPlan`) What a launch would compute for a plan, with
//! nothing persisted and nothing published: the SAME functions the launch runs — [`super::precheck`],
//! [`super::intent_score_for_run`] and [`super::decide`] over a fresh plan state — read back as
//! the intent score, the floor fill ([`crate::plan::FloorFilled`], field for field) and whether the
//! `plan_approval` gate would pause. The caller hands it what a launch would have: the repo's
//! root and the base commit its worktree would start from (`Core::preview_plan` resolves both the
//! way the launch does), and the launch's deliver step. Without a usable graph the plan scores as
//! a repo-less launch does (a behavioural touch set fails closed at 100), and `graph` says so.

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
    /// `"ready"`: the score read the repo's code graph at the base commit. `"not_needed"`: the
    /// touch set is docs-only (or the plan has no creator and no touch set), so no graph enters
    /// the score. `"unavailable"`: the score is the fail-closed one — no repo given, no graph
    /// indexed or a stale graph; `reasons` says which. `"pending_pa_scope"` (X1): a creator plan
    /// with no declared touch set — the launch runs the PA's read-only `pa-scope` step first and
    /// scores the plan from its answer, so `score` / `band` / `steps` are the baseline's (the
    /// lowest band), not a final score, and `pauses` is manual mode's (an auto-mode run may still
    /// pause once the PA's scope lands in high risk).
    pub graph: &'static str,
}

/// Preview `plan` as a launch with `human_confirm` would decide it. `Err` carries the refusal the
/// launch would return (compose, provenance, the override in auto mode, …).
pub(crate) fn preview_plan(
    plan: &PlanSteps,
    human_confirm: &HumanConfirm,
    repo_root: Option<&std::path::Path>,
    base_commit: Option<&str>,
    deliver_step: Option<&PlanStep>,
) -> anyhow::Result<PlanPreview> {
    // The launch's synchronous checks, then its decision — the same calls, in the same order.
    if let Err(r) = super::precheck(plan, deliver_step, human_confirm) {
        anyhow::bail!("the plan is refused: {}", r.reason);
    }
    // (X1) A plan the PA will scope has no score yet: the launch runs its scope step first and
    // decides the plan from the PA's answer. The preview shows that plan (the scope step first)
    // floor-filled at the baseline, and says the score is pending — never 100 as if final.
    let scoping = super::needs_pa_scope(plan);
    let unbound = repo_root.is_none();
    let (plan, scored) = if scoping {
        (
            super::with_scope_step(plan, unbound),
            super::scope::pending_scored(unbound),
        )
    } else {
        (
            plan.clone(),
            super::intent_score_for_run(plan, repo_root, base_commit),
        )
    };
    let plan = &plan;
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
        &super::TeamPlanState {
            deliver_step: deliver_step.cloned(),
            ..super::TeamPlanState::default()
        },
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
    // Did the score need the repo's graph, and could it read it? A docs-only (or absent, for a
    // plan with no creator) touch set needs none; a behavioural one reads it or fails closed.
    let behavioural = plan.touch.as_ref().is_some_and(|t| {
        let t: Vec<&str> = t.iter().map(String::as_str).collect();
        !t.is_empty() && crate::review_scale::signals_from_paths(&t).behavioural()
    });
    let graph = match (a.signals.is_some(), behavioural) {
        _ if scoping => super::scope::PENDING_PA_SCOPE,
        (true, true) => "ready",
        (true, false) => "not_needed",
        (false, _) => "unavailable",
    };
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
        graph,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn plan(v: Value) -> PlanSteps {
        serde_json::from_value(v).unwrap()
    }

    fn preview_of(p: &PlanSteps, hc: &HumanConfirm) -> anyhow::Result<PlanPreview> {
        preview_plan(p, hc, None, None, None)
    }

    fn preview(v: Value, hc: HumanConfirm) -> Value {
        serde_json::to_value(preview_of(&plan(v), &hc).expect("a preview")).unwrap()
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
                "graph",
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
        // A docs-only touch set: no graph enters the score.
        assert_eq!(p["graph"], "not_needed");
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

    /// (X1) A creator plan with no declared scope is scored by its PA after launch: the preview
    /// says the score is PENDING (`graph: "pending_pa_scope"`) instead of showing the fail-closed
    /// 100 as if final — the baseline's band and floor, the PA's read-only `pa-scope` step first, no
    /// auto-mode pause yet (manual mode always pauses).
    #[test]
    fn an_undeclared_creator_plan_previews_as_pending_the_pa_scope() {
        let p = preview(json!({"steps": [{"catalog": "build"}]}), HumanConfirm::None);
        assert_eq!(p["graph"], "pending_pa_scope");
        assert_eq!(p["score"], 0);
        assert_ne!(p["score"], 100, "never the fail-closed score as if final");
        assert_eq!(p["band"], "0-19");
        assert_eq!(p["high_risk"], false);
        assert_eq!(p["pauses"], false);
        let reasons = p["reasons"].as_array().unwrap();
        assert!(
            reasons[0]
                .as_str()
                .unwrap()
                .starts_with("pending the PA's scope"),
            "{reasons:?}"
        );
        let steps: Vec<(&str, &str)> = p["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| (s["catalog"].as_str().unwrap(), s["id"].as_str().unwrap()))
            .collect();
        assert_eq!(steps, [("understand", "pa-scope"), ("build", "build")]);
        assert_eq!(p["def"]["phases"][0]["id"], "pa-scope");
        // Manual mode: the plan gate after the scope step, always.
        let m = preview(json!({"steps": [{"catalog": "build"}]}), HumanConfirm::All);
        assert_eq!(
            (
                m["graph"].clone(),
                m["pauses"].clone(),
                m["pause_reason"].clone()
            ),
            (json!("pending_pa_scope"), json!(true), json!("manual_mode"))
        );
        // A declared touch set with no graph is still the fail-closed score (not pending).
        let d = preview(
            json!({"steps": [{"catalog": "build"}], "touch": ["src/x.rs"]}),
            HumanConfirm::None,
        );
        assert_eq!(
            (
                d["graph"].clone(),
                d["score"].clone(),
                d["pause_reason"].clone()
            ),
            (json!("unavailable"), json!(100), json!("high_risk"))
        );
    }

    /// The launch's refusals are the preview's: an override in auto mode, an unknown catalog id,
    /// supplied provenance.
    #[test]
    fn the_launch_refusals_are_the_preview_refusals() {
        let auto = HumanConfirm::None;
        let e = preview_of(
            &plan(json!({"steps": [{"catalog": "build"}], "override": {"remove": ["review"], "reason": "x"}})),
            &auto,
        )
        .unwrap_err();
        assert!(e.to_string().contains("override in auto mode"), "{e}");
        assert!(preview_of(&plan(json!({"steps": [{"catalog": "nope"}]})), &auto).is_err());
        assert!(preview_of(
            &plan(json!({"steps": [{"catalog": "build", "added_by": "floor"}]})),
            &auto
        )
        .is_err());
        // (X1) An override needs a declared touch set (its floor is scored from it).
        let e = preview_of(
            &plan(json!({"steps": [{"catalog": "build"}],
                         "override": {"remove": ["design"], "reason": "x"}})),
            &HumanConfirm::All,
        )
        .unwrap_err();
        assert!(e.to_string().contains("needs a declared touch set"), "{e}");
        // In manual mode the override is recorded and the plan pauses for it.
        let p = preview(
            json!({"steps": [{"catalog": "build"}], "touch": ["src/x.rs"],
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

    /// (round 3) The launch's deliver step rides the preview: a delivering launch's floor carries
    /// `deliver`, and the step is the launcher's (a plan authoring its own is refused, as at launch).
    #[test]
    fn a_delivering_launch_previews_with_its_deliver_step() {
        let d: PlanStep = serde_json::from_value(json!({
            "catalog": "deliver", "id": "deliver", "executor": {"type": "tool", "cmd": ["true"]}
        }))
        .unwrap();
        let p = preview_plan(
            &plan(json!({"steps": [{"catalog": "build"}], "touch": ["README.md"]})),
            &HumanConfirm::All,
            None,
            None,
            Some(&d),
        )
        .unwrap();
        assert!(p.floor.contains(&"deliver".to_string()), "{:?}", p.floor);
        let last = p.steps.last().unwrap();
        assert_eq!(
            (last.catalog.as_str(), last.id.as_str()),
            ("deliver", "deliver")
        );
        assert!(preview_plan(
            &plan(json!({"steps": [{"catalog": "build"},
                {"catalog": "deliver", "id": "deliver", "executor": {"type": "tool", "cmd": ["true"]}}]})),
            &HumanConfirm::All,
            None,
            None,
            Some(&d),
        )
        .is_err());
    }
}
