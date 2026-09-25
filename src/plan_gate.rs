//! Seam T3 (DES-TEAMING-002 §8.4-§8.6): the plan approval gate.
//!
//! Every plan — a user plan on the launch, a preset expanded at launch, a PA plan, an approval
//! edit — goes through ONE pipeline, [`decide`]: `plan.proposed` → the intent score
//! (`path.scored`) → floor fill (§8.5) → compose (§8.3) → the approval matrix (§8.6) →
//! `plan.accepted{by:"engine"}` or a plan HELD for a `plan_approval` gate. The actor persists the
//! state ([`TeamPlanState`] on `AgentSession.team_plan`), pauses at the step boundary, and
//! resolves the gate in `confirm_gate` ([`approve_pending`] / an edit through [`decide`]).
//!
//! **Computed, never read.** The score, the band, `high_risk` and whether auto mode may skip the
//! gate are computed here from the plan's declared touch set, the graph and the run's autonomy
//! (`HumanConfirm::None` ⇔ auto). Nothing on a plan or a launch can supply them. A plan whose
//! score cannot be computed scores `no_graph_score` (100), so a missing graph, repo or base commit
//! lands in the high-risk band and pauses: positive evidence only.
//!
//! **Publishing.** Every fact goes through [`publish`], the ONE call site: the thinnest hand-off
//! to the engine's existing emit path (`CoreEvent::TeamFact`, carrying exactly the bus row
//! `TeamEvent::bus_emit` builds). It rebases onto P1's `TeamBus::publish`.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::HumanConfirm;
use crate::plan::{AddedBy, FloorOverride, PlanRefusal, PlanStep, PlanSteps};
use crate::review_scale::{Assessment, Graph};
use crate::team::events::{
    self as ev, ApprovalReason, GateDecision, ProposalKind, ProposalSource, TeamEvent,
};
use crate::workflow::WorkflowDef;

/// The `gate_kind` token of a plan approval pause (`AwaitingHuman.gate_kind`, the durable
/// interaction row, `gate.opened.kind`).
pub(crate) const GATE_KIND: &str = "plan_approval";

/// `plan.refused.reason` for a floor override in auto mode (§8.5), spelled as DES-TEAMING-002
/// spells it.
pub(crate) const OVERRIDE_IN_AUTO: &str = "override in auto mode";

/// A run's plan state (DES-TEAMING-002 §8.4-§8.6), persisted on `AgentSession.team_plan` so a
/// restart keeps a `plan_approval` gate open and answerable. Every field is engine-computed.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TeamPlanState {
    /// The last `plan_rev` assigned (accepted or held). `0` = none yet.
    pub rev: u32,
    /// The last ACCEPTED `plan_rev`. `0` = no plan accepted yet.
    pub accepted_rev: u32,
    /// The accepted rev was high risk (§8.6's revision rows read it).
    pub accepted_high_risk: bool,
    /// A human approved a high-risk rev of this run (§8.6: a revision that stays high risk then
    /// proceeds in auto mode).
    pub approved_high_risk: bool,
    /// The ratcheted score (§8.5): the maximum any score of the run has reached. Only goes up.
    pub max_score: u8,
    /// A destructive signal was seen (§8.5: high risk in any band). Only goes up.
    pub destructive: bool,
    /// The preset the launch named, when it named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// The composed plan held for approval; `None` when nothing is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<PendingPlan>,
    /// The launch roster, so an edit accepted at the gate re-plans and re-distributes onto the
    /// same seats after a restart (the session keeps only the seat keys).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roster: Vec<Value>,
    /// The run DELIVERS (`LaunchSpec.deliver_step`): the launcher's `deliver` step, appended to
    /// every plan of the run that has none of its own, and the command that puts `deliver` in the
    /// floor (§8.5). `None` for a run that does not deliver.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deliver_step: Option<PlanStep>,
}

/// A composed, floor-filled plan held at a `plan_approval` gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPlan {
    /// The `plan_rev` it is accepted as when approved as-is.
    pub rev: u32,
    /// The `plan.proposed` it composes.
    pub proposal_id: String,
    /// The unit whose output produced the plan; `None` for a plan the launch carried.
    pub reviewing_ord: Option<u32>,
    /// The composed steps, each with `added_by` (and `floor_reason` when the floor added it).
    pub steps: PlanSteps,
    /// The floor band (`"70-100"`).
    pub band: String,
    /// §8.5's high-risk rule, computed.
    pub high_risk: bool,
    /// The floor override as recorded (manual mode only).
    pub floor_override: Option<FloorOverride>,
    /// Why approval is required: the `gate.opened.reason` token (`manual_mode`, `high_risk`,
    /// `into_high_risk`, `override`).
    pub reason: String,
    /// The catalog ids the floor added (`gate.opened.diff.added`).
    pub floor_added: Vec<String>,
    /// The gate this plan is held at (`g-<run>-<gate_seq>`), once opened. `None` until the step
    /// boundary opens it, and again after a refused edit (the re-opened gate gets a new id).
    pub gate_id: Option<String>,
    /// The last refused edit's reason, shown in the re-opened gate's prompt.
    pub refusal: Option<String>,
}

/// Which row of the approval matrix a plan event is (§8.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PlanEvent {
    /// The run's first plan (no rev accepted yet).
    Initial,
    /// A revision of an accepted rev.
    Revision {
        /// The accepted rev was high risk.
        previous_high_risk: bool,
        /// A human approved a high-risk rev of this run.
        approved_high_risk: bool,
    },
}

/// Auto mode ⇔ `HumanConfirm::None` (§8.6: the existing run-level autonomy, no second knob).
pub(crate) fn is_auto(hc: &HumanConfirm) -> bool {
    matches!(hc, HumanConfirm::None)
}

/// The approval matrix (§8.6), row by row. `Ok(None)` proceeds, `Ok(Some(reason))` requires
/// approval, `Err(reason)` refuses the plan. Every input is computed by the engine.
pub(crate) fn approval(
    auto: bool,
    high_risk: bool,
    has_override: bool,
    event: PlanEvent,
) -> Result<Option<ApprovalReason>, &'static str> {
    if has_override {
        // "A plan carrying a floor override: manual approval; auto refused."
        return if auto {
            Err(OVERRIDE_IN_AUTO)
        } else {
            Ok(Some(ApprovalReason::Override))
        };
    }
    if !auto {
        return Ok(Some(ApprovalReason::ManualMode));
    }
    Ok(match event {
        PlanEvent::Initial => high_risk.then_some(ApprovalReason::HighRisk),
        PlanEvent::Revision {
            previous_high_risk,
            approved_high_risk,
        } => match (high_risk, previous_high_risk, approved_high_risk) {
            (false, _, _) => None,
            (true, false, _) => Some(ApprovalReason::IntoHighRisk),
            (true, true, true) => None,
            // High risk that no human ever approved still needs one.
            (true, true, false) => Some(ApprovalReason::HighRisk),
        },
    })
}

/// A step with no `id` takes its catalog id (`{"catalog":"build"}` is a complete step, as T2 (g)
/// writes it); a repeat takes `<catalog>-2`, `-3`, …, skipping any id already used. An authored
/// id is never changed.
pub(crate) fn with_default_ids(plan: &PlanSteps) -> PlanSteps {
    let mut out = plan.clone();
    let mut used: HashSet<String> = plan
        .steps
        .iter()
        .filter(|s| !s.id.is_empty())
        .map(|s| s.id.clone())
        .collect();
    for s in out.steps.iter_mut().filter(|s| s.id.is_empty()) {
        let (mut id, mut n) = (s.catalog.clone(), 2);
        while used.contains(&id) {
            id = format!("{}-{n}", s.catalog);
            n += 1;
        }
        used.insert(id.clone());
        s.id = id;
    }
    out
}

/// The `deliver` step a delivering run's plans carry (§8.5): the catalog `deliver` entry, phase id
/// `deliver` (the engine's deliver gate keys on it), with a non-empty Tool command.
pub(crate) fn check_deliver_step(step: &PlanStep) -> Result<(), String> {
    let tool = matches!(
        &step.executor,
        Some(crate::workflow::PhaseExecutor::Tool { cmd }) if !cmd.is_empty()
    );
    if step.catalog == "deliver" && step.id == "deliver" && tool {
        Ok(())
    } else {
        Err(format!(
            "the deliver step must be the catalog `deliver` entry with id `deliver` and a Tool \
             command (got catalog `{}`, id `{}`)",
            step.catalog, step.id
        ))
    }
}

/// `plan` with the run's `deliver` step appended when it has no `deliver` step of its own.
fn with_deliver(plan: &PlanSteps, deliver: Option<&PlanStep>) -> PlanSteps {
    let mut out = plan.clone();
    if let Some(d) = deliver {
        if !out.steps.iter().any(|s| s.catalog == "deliver") {
            out.steps.push(d.clone());
        }
    }
    out
}

/// The deliver step's command: what `FloorInput.deliver` puts in the floor.
fn deliver_cmd(step: Option<&PlanStep>) -> Option<Vec<String>> {
    match step.map(|s| &s.executor) {
        Some(Some(crate::workflow::PhaseExecutor::Tool { cmd })) => Some(cmd.clone()),
        _ => None,
    }
}

/// A plan's intent score (§8.2, §8.4) and the destructive signal floor fill reads.
pub(crate) struct Scored {
    pub assessment: Assessment,
    /// Positive evidence of a destructive change: a destructive touched PATH (known without a
    /// graph) or the graph's own signal.
    pub destructive: bool,
}

/// Score a plan from its declared touch set (S4 via `assess_intent`): a creator plan with no
/// `touch` scores `no_graph_score` (100, "no declared scope"); a declared set is read against the
/// graph, failing closed at 100 when the graph is unusable; a plan with no creator step and no
/// `touch` scores 0.
pub(crate) fn intent_score(plan: &PlanSteps, graph: Graph<'_>) -> Scored {
    let touch: Option<Vec<&str>> = plan
        .touch
        .as_ref()
        .map(|t| t.iter().map(String::as_str).collect());
    let assessment =
        crate::review_scale::assess_intent(plan.has_creator(), touch.as_deref(), graph, None);
    let by_path = touch
        .as_deref()
        .is_some_and(|t| crate::review_scale::signals_from_paths(t).destructive);
    let by_graph = assessment.signals.as_ref().is_some_and(|s| s.destructive);
    Scored {
        assessment,
        destructive: by_path || by_graph,
    }
}

/// [`intent_score`] against the run's repo graph: the code graph indexed for `repo_root`, read AT
/// `base_commit` (S4's `graph_age` check). No repo, no base commit, no graph or an unreadable one
/// is `Graph::Unavailable` with the reason, so a declared touch set fails closed at 100.
pub(crate) fn intent_score_for_run(
    plan: &PlanSteps,
    repo_root: Option<&Path>,
    base_commit: Option<&str>,
) -> Scored {
    let unavailable = |why: String| intent_score(plan, Graph::Unavailable(why));
    if plan.touch.as_ref().is_none_or(|t| t.is_empty()) {
        return unavailable("no declared scope".into());
    }
    let Some(root) = repo_root else {
        return unavailable("the run has no repo, so there is no graph to read".into());
    };
    let Some(base) = base_commit else {
        return unavailable("the run's base commit is unknown".into());
    };
    let Some(db) = crate::code_graph::existing_code_graph(root) else {
        return unavailable(format!("no code graph is indexed for {}", root.display()));
    };
    match wicked_apps_core::open_store_ro(Some(&db.to_string_lossy())) {
        Ok(store) => intent_score(
            plan,
            Graph::Ready {
                store: &store,
                base_commit: base,
            },
        ),
        Err(e) => unavailable(format!("the code graph could not be opened: {e}")),
    }
}

/// One plan proposal, from any author (§8.4).
pub(crate) struct Proposal {
    /// `"human"` or the PA seat (`"claude#1"`).
    pub by: String,
    /// The id the engine received with the command that carried the plan (§6.1 row 3).
    pub source: ProposalSource,
    pub kind: ProposalKind,
    /// The preset the launch named.
    pub preset: Option<String>,
    pub plan: PlanSteps,
    /// The unit whose output produced the plan (a PA plan); `None` for a launch plan.
    pub reviewing_ord: Option<u32>,
    /// An edit made AT the approval gate: the human who edited it approved it (§8.6), so it is
    /// accepted directly unless refused.
    pub approved_by_human: bool,
}

/// What [`decide`] did with a proposal.
pub(crate) enum Verdict {
    /// Accepted as `state.rev`; `def` is the composed per-run def `<run>:plan-<rev>`.
    Accepted { def: WorkflowDef },
    /// Held for approval as `state.rev` (`state.pending`); `def` is what approval releases.
    Held { def: WorkflowDef },
    /// Refused (`plan.refused`); the run keeps its prior state.
    Refused { reason: String },
}

pub(crate) struct Decided {
    /// The run's plan state after the proposal (the prior state when refused).
    pub state: TeamPlanState,
    /// The facts to publish, in order.
    pub events: Vec<TeamEvent>,
    pub verdict: Verdict,
}

/// The per-run composed def id (§8.3): `<run>:plan-<rev>`, the shape
/// [`crate::plan::per_run_def_run_id`] recognizes as a TEAM run.
pub(crate) fn per_run_def_id(run_id: &str, rev: u32) -> String {
    format!("{run_id}:plan-{rev}")
}

/// `plan.refused.reason` for a refusal: free text naming the refusing rule.
fn refusal_text(r: &PlanRefusal) -> String {
    match r {
        PlanRefusal::OverrideInAutoMode => OVERRIDE_IN_AUTO.to_string(),
        other => other.to_string(),
    }
}

/// The one pipeline (§8.4): publish the proposal, score it, ratchet, floor-fill and compose it,
/// then the approval matrix. Pure: the caller persists `state` and publishes `events`.
pub(crate) fn decide(
    run_id: &str,
    proposal: Proposal,
    prior: &TeamPlanState,
    human_confirm: &HumanConfirm,
    scored: &Scored,
    now: i64,
) -> anyhow::Result<Decided> {
    let auto = is_auto(human_confirm);
    let plan = with_default_ids(&with_deliver(&proposal.plan, prior.deliver_step.as_ref()));
    let deliver = deliver_cmd(prior.deliver_step.as_ref());
    let proposal_id = ev::mint_proposal_id(run_id, &proposal.by, &proposal.source);
    let base_rev = (prior.accepted_rev > 0).then_some(prior.accepted_rev);
    let mut events = vec![
        plan_proposed(
            run_id,
            &proposal.by,
            &proposal_id,
            base_rev,
            proposal.kind,
            proposal.preset.as_deref(),
            &plan,
            now,
        )?,
        path_scored(run_id, &proposal_id, &scored.assessment, now)?,
    ];
    let refuse = |mut events: Vec<TeamEvent>, reason: String| -> anyhow::Result<Decided> {
        events.push(plan_refused(run_id, &proposal_id, base_rev, &reason, now)?);
        Ok(Decided {
            state: prior.clone(),
            events,
            verdict: Verdict::Refused { reason },
        })
    };
    // §8.5 ratchet: the floor band is the maximum any score of the run has reached.
    let max_score = prior.max_score.max(scored.assessment.score);
    let destructive = prior.destructive || scored.destructive;
    let filled = match crate::plan::floor_fill(
        crate::catalog::catalog(),
        &plan,
        crate::plan::FloorInput {
            score: max_score,
            destructive,
            human_confirm,
            deliver: deliver.as_deref(),
        },
    ) {
        Ok(f) => f,
        Err(r) => return refuse(events, refusal_text(&r)),
    };
    let rev = prior.rev + 1;
    let mut def = filled.def.clone();
    def.id = per_run_def_id(run_id, rev);
    let event = if prior.accepted_rev == 0 {
        PlanEvent::Initial
    } else {
        PlanEvent::Revision {
            previous_high_risk: prior.accepted_high_risk,
            approved_high_risk: prior.approved_high_risk,
        }
    };
    let needs = approval(
        auto,
        filled.high_risk,
        filled.floor_override.is_some(),
        event,
    );
    let needs = match needs {
        Err(reason) => return refuse(events, reason.to_string()),
        // The human who edited the plan at the gate approved it (§8.6 "approve with amend").
        Ok(_) if proposal.approved_by_human => None,
        Ok(n) => n,
    };
    let mut state = prior.clone();
    state.rev = rev;
    state.max_score = max_score;
    state.destructive = destructive;
    if proposal.preset.is_some() {
        state.preset = proposal.preset.clone();
    }
    match needs {
        None => {
            let by = if proposal.approved_by_human {
                "human"
            } else {
                "engine"
            };
            events.push(plan_accepted(
                run_id,
                by,
                rev,
                &filled.band,
                filled.high_risk,
                auto,
                &filled.steps,
                filled.floor_override.as_ref(),
                &proposal_id,
                now,
            )?);
            state.accepted_rev = rev;
            state.accepted_high_risk = filled.high_risk;
            state.approved_high_risk |= proposal.approved_by_human && filled.high_risk;
            state.pending = None;
            Ok(Decided {
                state,
                events,
                verdict: Verdict::Accepted { def },
            })
        }
        Some(reason) => {
            state.pending = Some(PendingPlan {
                rev,
                proposal_id,
                reviewing_ord: proposal.reviewing_ord,
                floor_added: filled
                    .steps
                    .steps
                    .iter()
                    .filter(|s| s.added_by == Some(AddedBy::Floor))
                    .map(|s| s.catalog.clone())
                    .collect(),
                steps: filled.steps,
                band: filled.band,
                high_risk: filled.high_risk,
                floor_override: filled.floor_override,
                reason: reason.as_str().to_string(),
                gate_id: None,
                refusal: None,
            });
            Ok(Decided {
                state,
                events,
                verdict: Verdict::Held { def },
            })
        }
    }
}

/// The plan a launch carries (§8.4): its user-composed `plan`, or the steps of the preset its
/// `workflow` names (with the preset's name). `None` when it carries neither (a registered def or
/// the prose planner). A launch naming both is refused: a plan or a preset, never two plans.
pub(crate) fn launch_plan(
    store: &dyn wicked_apps_core::GraphRead,
    plan: Option<&PlanSteps>,
    workflow: Option<&str>,
    project_id: Option<&str>,
) -> anyhow::Result<Option<(PlanSteps, Option<String>)>> {
    match (plan, workflow) {
        (Some(_), Some(w)) => {
            anyhow::bail!("a launch carries a plan or names a preset (`{w}`), not both — drop one")
        }
        (Some(p), None) => Ok(Some((p.clone(), None))),
        (None, Some(w)) => Ok(crate::preset::resolve(store, project_id, w)?.map(|p| {
            (
                PlanSteps {
                    steps: p.steps,
                    ..PlanSteps::default()
                },
                Some(p.name),
            )
        })),
        (None, None) => Ok(None),
    }
}

/// A launch plan refused before the run exists, with the facts that say so.
pub(crate) struct Refusal {
    pub reason: String,
    pub events: Vec<TeamEvent>,
}

/// The launch-time checks that do not depend on the score (§8.4-§8.5): supplied provenance, a
/// floor override in auto mode, and the authored steps composing over the catalog. A refusal is
/// a synchronous launch error with `plan.proposed` + `plan.refused` published; everything that
/// depends on the score is judged once the worktree's base commit is known ([`decide`]).
pub(crate) fn precheck(
    run_id: &str,
    plan: &PlanSteps,
    preset: Option<&str>,
    deliver_step: Option<&PlanStep>,
    human_confirm: &HumanConfirm,
    now: i64,
) -> Result<(), Refusal> {
    let plan = with_default_ids(&with_deliver(plan, deliver_step));
    let reason = if let Some(Err(why)) = deliver_step.map(check_deliver_step) {
        Some(why)
    } else if let Some(s) = plan
        .steps
        .iter()
        .find(|s| s.added_by.is_some() || s.floor_reason.is_some())
    {
        Some(refusal_text(&PlanRefusal::ProvenanceSupplied {
            step: s.id.clone(),
            catalog: s.catalog.clone(),
        }))
    } else if plan.floor_override.is_some() && is_auto(human_confirm) {
        Some(OVERRIDE_IN_AUTO.to_string())
    } else {
        crate::plan::compose(crate::catalog::catalog(), &plan)
            .err()
            .map(|r| refusal_text(&r))
    };
    let Some(reason) = reason else {
        return Ok(());
    };
    let proposal_id = ev::mint_proposal_id(
        run_id,
        "human",
        &ProposalSource::Launch {
            session_id: run_id.to_string(),
        },
    );
    let events = [
        plan_proposed(
            run_id,
            "human",
            &proposal_id,
            None,
            ProposalKind::Initial,
            preset,
            &plan,
            now,
        ),
        plan_refused(run_id, &proposal_id, None, &reason, now),
    ]
    .into_iter()
    .filter_map(Result::ok)
    .collect();
    Err(Refusal { reason, events })
}

/// The launch plan's AUTHORED steps composed as rev 1 (`<run>:plan-1`), for the launch-time
/// checks that read a def (tool preflight, base skill, seat need). The floor is added later.
pub(crate) fn authored_def(
    run_id: &str,
    plan: &PlanSteps,
    deliver_step: Option<&PlanStep>,
) -> anyhow::Result<WorkflowDef> {
    let plan = with_default_ids(&with_deliver(plan, deliver_step));
    let mut def = crate::plan::compose(crate::catalog::catalog(), &plan)
        .map_err(|r| anyhow::anyhow!("{r}"))?;
    def.id = per_run_def_id(run_id, 1);
    Ok(def)
}

/// Approve the held plan as-is at its gate (§8.6): `gate.decided{human_approved}` then
/// `plan.accepted{by:"human"}`, and the state with the rev accepted.
pub(crate) fn approve_pending(
    run_id: &str,
    state: &TeamPlanState,
    human_confirm: &HumanConfirm,
    ord: u32,
    attempt: u32,
    now: i64,
) -> anyhow::Result<(TeamPlanState, Vec<TeamEvent>)> {
    let p = state
        .pending
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("run {run_id} holds no plan for approval"))?;
    let gate_id = p
        .gate_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("run {run_id}'s plan gate never opened"))?;
    let events = vec![
        gate_decided(
            run_id,
            &gate_id,
            ord,
            attempt,
            GateDecision::HumanApproved,
            now,
        )?,
        plan_accepted(
            run_id,
            "human",
            p.rev,
            &p.band,
            p.high_risk,
            is_auto(human_confirm),
            &p.steps,
            p.floor_override.as_ref(),
            &p.proposal_id,
            now,
        )?,
    ];
    let mut next = state.clone();
    next.accepted_rev = p.rev;
    next.accepted_high_risk = p.high_risk;
    next.approved_high_risk |= p.high_risk;
    next.pending = None;
    Ok((next, events))
}

/// The prompt a `plan_approval` gate shows (§8.5: the override is shown at the gate).
pub(crate) fn gate_prompt(p: &PendingPlan, ord: u32, auto: bool) -> String {
    let steps: Vec<String> = p
        .steps
        .steps
        .iter()
        .map(|s| match s.added_by {
            Some(AddedBy::Floor) => format!("{} (floor)", s.id),
            _ => s.id.clone(),
        })
        .collect();
    let why = match p.reason.as_str() {
        "high_risk" => "high risk: auto mode still requires approval",
        "into_high_risk" => "the plan moved into high risk",
        "override" => "the plan overrides the floor",
        _ => "manual mode",
    };
    let mut out = format!(
        "Approve plan rev {} before unit {ord} runs ({why}; band {}{}; {} mode): {}",
        p.rev,
        p.band,
        if p.high_risk { ", high risk" } else { "" },
        if auto { "auto" } else { "manual" },
        steps.join(" → ")
    );
    if !p.floor_added.is_empty() {
        out.push_str(&format!(". Floor added: {}", p.floor_added.join(", ")));
    }
    if let Some(o) = &p.floor_override {
        out.push_str(&format!(
            ". Floor override: removes {} — {}",
            o.remove.join(", "),
            o.reason
        ));
    }
    if let Some(r) = &p.refusal {
        out.push_str(&format!(". The previous edit was refused: {r}"));
    }
    out.push_str(". Approve, approve with an edited plan, or reject.");
    out
}

// ── Event builders: each payload goes through `TeamEvent::from_payload`, so what is published is
// exactly the T1 wire contract (computed fields recomputed, keys from the one rule). ──────────────

fn envelope(run_id: &str, by: &str, ord: Option<u32>, attempt: Option<u32>, now: i64) -> Value {
    json!({"run_id": run_id, "ord": ord, "attempt": attempt, "by": by, "at": now, "re": null})
}

fn build(event_type: &str, env: Value, body: Value) -> anyhow::Result<TeamEvent> {
    let (Value::Object(mut out), Value::Object(body)) = (env, body) else {
        anyhow::bail!("a team payload is an object");
    };
    out.extend(body);
    TeamEvent::from_payload(event_type, &Value::Object(out))
}

/// A plan step on the wire (§6 `plan.proposed.steps`): what the plan named, unset fields omitted.
fn wire_step(s: &PlanStep) -> Value {
    let mut v = json!({"catalog": s.catalog, "id": s.id});
    let o = v.as_object_mut().expect("object");
    if let Some(i) = &s.instructions {
        o.insert("instructions".into(), json!(i));
    }
    if let Some(w) = &s.owner {
        o.insert("owner".into(), json!(w));
    }
    if let Some(d) = &s.depends_on {
        o.insert("depends_on".into(), json!(d));
    }
    if let Some(g) = &s.gate {
        o.insert("gate".into(), json!(g));
    }
    if let Some(a) = &s.added_by {
        o.insert("added_by".into(), json!(a));
    }
    if let Some(r) = &s.floor_reason {
        o.insert("floor_reason".into(), json!(r));
    }
    v
}

fn wire_steps(p: &PlanSteps) -> Vec<Value> {
    p.steps.iter().map(wire_step).collect()
}

#[allow(clippy::too_many_arguments)]
fn plan_proposed(
    run_id: &str,
    by: &str,
    proposal_id: &str,
    base_rev: Option<u32>,
    kind: ProposalKind,
    preset: Option<&str>,
    plan: &PlanSteps,
    now: i64,
) -> anyhow::Result<TeamEvent> {
    build(
        ev::PLAN_PROPOSED,
        envelope(run_id, by, None, None, now),
        json!({
            "proposal_id": proposal_id,
            "base_rev": base_rev,
            "kind": kind,
            "preset": preset,
            "steps": wire_steps(plan),
            "monitors": {"asked": 0},
            "asks": [],
            "touch": plan.touch.clone().unwrap_or_default(),
            "override": plan.floor_override,
            "rationale": "",
        }),
    )
}

fn path_scored(
    run_id: &str,
    proposal_id: &str,
    a: &Assessment,
    now: i64,
) -> anyhow::Result<TeamEvent> {
    let signals = a.signals.as_ref().map(|s| {
        json!({
            "changed_symbols": s.changed_symbols, "dependents": s.dependents,
            "products": s.products, "contract_change": s.contract_change, "test_gap": s.test_gap,
            "critical": s.critical, "destructive": s.destructive, "truncated": s.truncated,
        })
    });
    let model = a
        .model
        .as_ref()
        .map(|m| json!({"add": m.add, "rationale": m.rationale}));
    build(
        ev::PATH_SCORED,
        envelope(run_id, "engine", None, None, now),
        json!({
            "score_source": ev::score_source_intent(proposal_id),
            "basis": "intent",
            "deterministic": a.deterministic,
            "reasons": a.reasons,
            "model": model,
            "signals": signals,
            "tree": null,
        }),
    )
}

#[allow(clippy::too_many_arguments)]
fn plan_accepted(
    run_id: &str,
    by: &str,
    rev: u32,
    band: &str,
    high_risk: bool,
    auto: bool,
    steps: &PlanSteps,
    floor_override: Option<&FloorOverride>,
    proposal_id: &str,
    now: i64,
) -> anyhow::Result<TeamEvent> {
    build(
        ev::PLAN_ACCEPTED,
        envelope(run_id, by, None, None, now),
        json!({
            "plan_rev": rev,
            "workflow_id": per_run_def_id(run_id, rev),
            "band": band,
            "high_risk": high_risk,
            "mode": if auto { "auto" } else { "manual" },
            "steps": wire_steps(steps),
            "override": floor_override,
            "proposal_id": proposal_id,
        }),
    )
}

fn plan_refused(
    run_id: &str,
    proposal_id: &str,
    base_rev: Option<u32>,
    reason: &str,
    now: i64,
) -> anyhow::Result<TeamEvent> {
    build(
        ev::PLAN_REFUSED,
        envelope(run_id, "engine", None, None, now),
        json!({"proposal_id": proposal_id, "base_rev": base_rev, "reason": reason}),
    )
}

/// `gate.opened{kind:"plan_approval"}` for the held plan (§6 row 23).
#[allow(clippy::too_many_arguments)]
pub(crate) fn gate_opened(
    run_id: &str,
    gate_id: &str,
    ord: u32,
    attempt: u32,
    p: &PendingPlan,
    auto: bool,
    from_rev: Option<u32>,
    now: i64,
) -> anyhow::Result<TeamEvent> {
    build(
        ev::GATE_OPENED,
        envelope(run_id, "engine", Some(ord), Some(attempt), now),
        json!({
            "gate_id": gate_id,
            "kind": GATE_KIND,
            "reviewing_ord": p.reviewing_ord,
            "plan_rev": p.rev,
            "band": p.band,
            "high_risk": p.high_risk,
            "mode": if auto { "auto" } else { "manual" },
            "reason": p.reason,
            "diff": {"from_rev": from_rev, "added": p.floor_added},
        }),
    )
}

/// `gate.decided{kind:"plan_approval"}`: a human decision, published by the engine after
/// `confirm_gate` accepts it (§4.0).
pub(crate) fn gate_decided(
    run_id: &str,
    gate_id: &str,
    ord: u32,
    attempt: u32,
    decision: GateDecision,
    now: i64,
) -> anyhow::Result<TeamEvent> {
    let mut env = envelope(run_id, "human", Some(ord), Some(attempt), now);
    env["re"] = json!(format!("gate.opened#{gate_id}"));
    build(
        ev::GATE_DECIDED,
        env,
        json!({
            "gate_id": gate_id,
            "kind": GATE_KIND,
            "decision": decision,
            "combined": null,
            "team_pause": false,
            "unresolved": [],
        }),
    )
}

/// THE one call site every T3 fact goes through: the thinnest hand-off to the engine's existing
/// emit path. The fact rides `CoreEvent::TeamFact` as exactly the bus row `TeamEvent::bus_emit`
/// builds (type, key, payload). Rebases onto P1's `TeamBus::publish`.
pub(crate) fn publish(sink: &mut crate::event_log::EventSink, fact: &TeamEvent) {
    let run = fact.env.run_id.clone();
    match fact.bus_emit() {
        Ok(row) => sink.emit(crate::CoreEvent::TeamFact {
            session: run,
            event_type: row.event_type,
            key: row.idempotency_key.unwrap_or_default(),
            payload: row.payload,
        }),
        Err(e) => sink.emit(crate::CoreEvent::Error {
            session: Some(run),
            message: format!("team fact {} could not be built: {e}", fact.event_type()),
        }),
    }
}

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
            assert!(matches!(d.verdict, Verdict::Accepted { .. }), "{by}");
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
            assert!(matches!(d.verdict, Verdict::Held { .. }), "{by}");
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
