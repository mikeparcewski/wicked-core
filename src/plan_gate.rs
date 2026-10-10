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
//! **Publishing.** This module is pure: it BUILDS the facts. The actor publishes them through
//! P1's one path (`TeamBus::publish` via the actor's publisher link, `actor::team_gate`): the
//! launch's `plan.proposed` / `path.scored` are queued on the plan state and follow the run's
//! `path.started` onto the bus; `plan.accepted` (from [`AcceptedPlan`]) and the plan gate's
//! `gate.decided` are P1 required transitions; `gate.opened` and `plan.refused` ride the FIFO. An
//! un-teamed run (no bus, `transport: none`) publishes none of them and its gate works the same.

use std::collections::HashSet;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::domain::HumanConfirm;
use crate::plan::{AddedBy, FloorOverride, HeldObligation, PlanRefusal, PlanStep, PlanSteps};
use crate::review_scale::{Assessment, Graph};
use crate::team::events::{
    self as ev, ApprovalReason, GateDecision, ProposalKind, ProposalSource, TeamEvent, TouchSource,
    ACCEPTED_TOUCH_CAP,
};
use crate::workflow::WorkflowDef;

mod preview;
mod revise;
mod scope;
pub(crate) use preview::preview_plan;
pub use preview::PlanPreview;
pub(crate) use revise::{
    changes_from_output, diff_score_for_run, floor_rises, hold_rescore, path_scored_diff,
    plan_lines_of, revise, with_scoring, Change, DiffRescore, Outcome, PlanLines, ScoreScope,
};
pub(crate) use scope::{
    decide_scoped, needs_pa_scope, scope_lines_of, scope_rev, with_scope_step, SCOPE_STEP_ID,
};
pub use scope::{ScopeAnswer, ScopeHold};
#[cfg(test)]
pub(crate) use scope::{PA_DECLARED_NO_SCOPE, SCOPED_NOTHING};

/// The `gate_kind` token of a plan approval pause (`AwaitingHuman.gate_kind`, the durable
/// interaction row, `gate.opened.kind`).
pub(crate) const GATE_KIND: &str = "plan_approval";

/// `plan.refused.reason` for a floor override in auto mode (§8.5), spelled as DES-TEAMING-002
/// spells it.
pub(crate) const OVERRIDE_IN_AUTO: &str = "override in auto mode";

/// (X1) The refusal of a floor override on a plan with no declared touch set.
pub(crate) const OVERRIDE_NEEDS_TOUCH: &str = "a floor override needs a declared touch set: the \
     floor it removes from is scored from it, and a plan with none is scored by its PA after launch";

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
    /// (X-MIG M9) The launch's declared deliverables (`LaunchSpec.deliverables`), kept so every
    /// plan [`decide`] judges carries them — a whole-plan edit at the initial approval gate cannot
    /// drop them (codex r1 on #858). Empty for a run that declared none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deliverables: Vec<String>,
    /// The accepted rev's `plan.accepted` body (a P1 required transition published before the
    /// rev's first dispatch). `None` while nothing is accepted (a plan held for approval).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted: Option<AcceptedPlan>,
    /// Facts built before the run's `path.started` landed (the launch's `plan.proposed`,
    /// `path.scored`): published, in order, right after it — never ahead of the run's path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub queued: Vec<QueuedFact>,
    /// The unit a `plan_approval` gate released: its next dispatch skips the human gates the
    /// approval already answered (the confirm path's "bypass `should_pause`"), once.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_ord: Option<u32>,
    /// (T4, §8.7) The highest diff re-score that raised the floor since the last step boundary:
    /// applied there as `plan.revised{reason:"floor_raised"}`, never mid-unit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rescored: Option<DiffRescore>,
    /// (T4, §8.7) The PA's `PLAN` lines from its finished turns (its own step or its review of a
    /// member's step), held until the run's next advance applies them — every advance goes
    /// through the one hook, whichever path (fold, dispute answer, member accept) led there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan_lines: Vec<PlanLines>,
    /// (T8 (c)) Mid-run human edits taken by `Core::propose_plan`, held — like the PA's `PLAN`
    /// lines — for the run's next advance, which applies them through the same revision path.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<HeldEdit>,
    /// Every request id `Core::propose_plan` took for this run: a repeat is a no-op, so one
    /// request id never proposes (or publishes) twice.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edit_requests: Vec<String>,
    /// (X1) The launch plan the PA is scoping: set at launch for a creator plan with no declared
    /// touch set (rev 1 is then the read-only scope step alone), taken at the scope step's
    /// boundary, where the plan is scored from the PA's answer and decided as the initial plan.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<ScopeHold>,
    /// (WT-C3, DES-walkthrough-proof §4.12) The obligations of every held testing rule the run's
    /// plans have fired, ratcheted like `max_score`: a rule that fired once binds every later rev.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub obligations: Vec<HeldObligation>,
    /// (WT-C3) Every advisory (recall-only) testing rule that applied to one of the run's plans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub recalled: Vec<String>,
}

/// (WT-C3, DES-walkthrough-proof §4.12) The testing rules the engine read for one proposal at
/// `plan.compose` (`wicked_governance::rules_at_phase` over the derived plan context): the
/// obligations of the held rules that fired and the advisory rules that applied. Loaded by the
/// actor (it holds the store); [`decide`] stays pure.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RulesEval {
    pub held: Vec<HeldObligation>,
    pub recalled: Vec<String>,
}

impl RulesEval {
    /// `rules` as read for a plan: a fired `allow_with_conditions` rule contributes its
    /// obligations; a recall-only rule is recorded as considered. A plan.compose rule is
    /// advisory or held — anything else is refused when written — so a fired row that is neither
    /// (a `deny` / `allow` effect, or a held rule with no obligation, written before the check or
    /// around it) becomes an obligation outside the vocabulary: floor fill refuses it naming the
    /// rule, at compose and at a re-score alike (fail closed, one mechanism).
    pub(crate) fn from_phase_rules(rules: &wicked_governance::PhaseRules) -> Self {
        let mut out = RulesEval {
            recalled: rules.recalled.clone(),
            ..Default::default()
        };
        for p in &rules.fired {
            let held = |token: &str| HeldObligation {
                rule: p.id.clone(),
                token: token.to_string(),
            };
            match p.effect {
                wicked_governance::Effect::AllowWithConditions if !p.obligations.is_empty() => {
                    out.held.extend(p.obligations.iter().map(|t| held(t)));
                }
                wicked_governance::Effect::AllowWithConditions => {
                    out.held.push(held("(no obligation)"))
                }
                wicked_governance::Effect::Deny => out.held.push(held("effect:deny")),
                wicked_governance::Effect::Allow => out.held.push(held("effect:allow")),
            }
        }
        out.recalled.sort();
        out.recalled.dedup();
        out
    }
}

/// (WT-C3) Where [`decide`] reads the `plan.compose` testing rules from: the actor's store for
/// the run's projects ([`StoreRules`]), or nothing ([`NoRules`]: a preview pending the PA's
/// scope, and the pure-pipeline tests).
pub(crate) trait RuleSource {
    /// The projects the run is filed in.
    fn projects(&self) -> &[String];
    /// The run is teamed on positive evidence, so its diff re-score corrects the declared touch.
    fn teamed(&self) -> bool;
    /// The rules for one plan context.
    fn eval(&self, context: &Value) -> anyhow::Result<RulesEval>;
}

/// No testing rules: every plan composes on the band floor alone (also a pending-scope preview,
/// whose rules are read only once the PA has scoped it).
pub(crate) struct NoRules;

impl RuleSource for NoRules {
    fn projects(&self) -> &[String] {
        &[]
    }
    fn teamed(&self) -> bool {
        false
    }
    fn eval(&self, _: &Value) -> anyhow::Result<RulesEval> {
        Ok(RulesEval::default())
    }
}

/// The testing rules in the engine's governance store for a run's projects.
pub(crate) struct StoreRules<'a> {
    pub store: &'a dyn wicked_apps_core::GraphRead,
    pub projects: Vec<String>,
    pub teamed: bool,
}

impl<'a> StoreRules<'a> {
    /// The rules for `run_id`: its `crew.run` project memberships read from the store (never
    /// env), plus the launch's own `project_id` (a launch is filed as it lands); a store error
    /// propagates (fail closed, never a silent narrowing).
    pub(crate) fn for_run(
        store: &'a dyn wicked_apps_core::GraphRead,
        run_id: &str,
        launch_project: Option<&str>,
        teamed: bool,
    ) -> anyhow::Result<Self> {
        let mut projects =
            crate::project::member_projects(store, crate::project::MEMBER_KIND_RUN, run_id)?;
        if let Some(p) = launch_project {
            if !projects.iter().any(|q| q == p) {
                projects.push(p.to_string());
            }
        }
        Ok(StoreRules {
            store,
            projects,
            teamed,
        })
    }
}

impl RuleSource for StoreRules<'_> {
    fn projects(&self) -> &[String] {
        &self.projects
    }
    fn teamed(&self) -> bool {
        self.teamed
    }
    fn eval(&self, context: &Value) -> anyhow::Result<RulesEval> {
        let rules = wicked_governance::rules_at_phase(
            self.store,
            wicked_governance::PLAN_COMPOSE_PHASE,
            &self.projects,
            context,
        )?;
        Ok(RulesEval::from_phase_rules(&rules))
    }
}

/// `prior` followed by every entry of `more` it lacks (first-seen order): the obligation and
/// recall ratchets only grow.
pub(crate) fn union<T: Clone + PartialEq>(prior: &[T], more: &[T]) -> Vec<T> {
    let mut out = prior.to_vec();
    for m in more {
        if !out.contains(m) {
            out.push(m.clone());
        }
    }
    out
}

/// (WT-C3) The engine-derived plan context a `plan.compose` rule's trigger reads
/// (DES-walkthrough-proof §4.12 B5): never declared by the plan. `kinds` is the set of `classify`
/// results over `paths` — failing closed to `["code"]` when there is no path or the run is not
/// (yet) teamed, since only a teamed run's diff re-score can correct an under-declared touch.
/// `project` is the first project the run is filed in (sorted), `null` for an unfiled run.
pub(crate) fn plan_context(
    projects: &[String],
    paths: &[String],
    teamed: bool,
    critical: bool,
    destructive: bool,
    band: &str,
    deliver: bool,
) -> Value {
    let kinds: Vec<&str> = if teamed && !paths.is_empty() {
        let mut k: Vec<&str> = paths
            .iter()
            .map(|p| crate::review_scale::kind_name(p))
            .collect();
        k.sort();
        k.dedup();
        k
    } else {
        vec!["code"]
    };
    let mut sorted = projects.to_vec();
    sorted.sort();
    json!({
        "project": sorted.first(),
        "kinds": kinds,
        "paths": paths.iter().take(PLAN_CONTEXT_PATHS_CAP).collect::<Vec<_>>(),
        "critical": critical,
        "destructive": destructive,
        "band": band,
        "deliver": deliver,
    })
}

/// At most this many paths ride a plan context (a trigger is a regex over its JSON).
pub(crate) const PLAN_CONTEXT_PATHS_CAP: usize = 200;

/// (WT-C3) `plan.accepted.rules` for a floor fill under `obligations` and `recalled`: each held
/// rule `applied`, or `overridden` when the override removed every type it requires (or
/// `recalled` when the plan changes nothing, so nothing was owed); each advisory rule
/// `recalled`. In id order.
pub(crate) fn rule_outcomes(
    obligations: &[HeldObligation],
    recalled: &[String],
    filled: &crate::plan::FloorFilled,
) -> Vec<crate::team::events::RuleOutcome> {
    use crate::team::events::{RuleOutcome, RuleOutcomeKind};
    let removed: &[String] = filled
        .floor_override
        .as_ref()
        .map_or(&[], |o| o.remove.as_slice());
    let mut out: Vec<RuleOutcome> = Vec::new();
    let mut held: Vec<&str> = obligations.iter().map(|o| o.rule.as_str()).collect();
    held.sort();
    held.dedup();
    for rule in held {
        // A token is disabled when the override removed ANY phase it requires (floor fill treats
        // either half of the walkthrough pair as the operator's own); the rule is `overridden`
        // when every token it holds is disabled.
        let disabled: Vec<bool> = obligations
            .iter()
            .filter(|o| o.rule == rule)
            .filter_map(|o| crate::plan::obligation_types(&o.token))
            .map(|types| types.iter().any(|t| removed.iter().any(|r| r == t)))
            .collect();
        let outcome = if filled.floor.is_empty() {
            RuleOutcomeKind::Recalled
        } else if !disabled.is_empty() && disabled.iter().all(|d| *d) {
            RuleOutcomeKind::Overridden
        } else {
            RuleOutcomeKind::Applied
        };
        out.push(RuleOutcome {
            id: rule.to_string(),
            outcome,
        });
    }
    for id in recalled {
        if !out.iter().any(|o| &o.id == id) {
            out.push(RuleOutcome {
                id: id.clone(),
                outcome: RuleOutcomeKind::Recalled,
            });
        }
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// A mid-run human edit (`Core::propose_plan`) waiting for the next step boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldEdit {
    /// Crew's per-POST request id: the `plan.proposed` source (§6.1 row 3).
    pub request_id: String,
    /// The steps to ADD (the plan only grows, §8.7).
    pub steps: Vec<PlanStep>,
}

/// What `Core::propose_plan` did with an edit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanProposal {
    /// The `plan.proposed` id the edit is published under (`"p-" + key([run, "human", request id])`).
    pub proposal_id: String,
    /// `true` when this request id was already taken: nothing new is held or published.
    pub duplicate: bool,
    /// What the edit does to the run, from the dry run it was validated by (the human's own edit
    /// opens no gate, so this is where they see it): the floor band and high-risk rule of the rev
    /// it makes, and the floor phase types it adds. `null` / empty for a duplicate.
    pub band: Option<String>,
    pub high_risk: Option<bool>,
    pub floor_added: Vec<String>,
}

/// An accepted plan rev: the body of its `plan.accepted` (§6 row 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcceptedPlan {
    pub rev: u32,
    /// `"engine"` (auto release) or `"human"` (approved or edited at the gate).
    pub by: String,
    pub band: String,
    pub high_risk: bool,
    pub auto: bool,
    pub steps: PlanSteps,
    pub floor_override: Option<FloorOverride>,
    pub proposal_id: String,
    /// (TR-W1a) The declared touch of this rev unioned with every earlier accepted rev's
    /// ([`AcceptedPlan::with_touch`]), at most `ACCEPTED_TOUCH_CAP` paths. Published on
    /// `plan.accepted.touch`; scoring never reads it. Old rows: empty.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touch: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub touch_truncated: bool,
    /// (TR-W1a) Where the touch came from; `None` on a row persisted before the field existed
    /// (published as `none`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touch_source: Option<TouchSource>,
    /// (WT-C3) The testing rules this rev was composed under (`plan.accepted.rules`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<crate::team::events::RuleOutcome>,
}

impl AcceptedPlan {
    /// (TR-W1a) Stamp this rev's `touch` / `touch_truncated` / `touch_source`: `prior`'s touch
    /// (the run's previously accepted rev, if any) followed by this rev's declared
    /// `steps.touch`, first-seen order, cut at `ACCEPTED_TOUCH_CAP`. `source` names where THIS
    /// rev's declared touch came from; a rev that declares none keeps `prior`'s source, and an
    /// empty union is `none`.
    pub(crate) fn with_touch(mut self, prior: Option<&AcceptedPlan>, source: TouchSource) -> Self {
        let mut union: Vec<String> = Vec::new();
        let mut truncated = prior.is_some_and(|p| p.touch_truncated);
        let own: &[String] = self.steps.touch.as_deref().unwrap_or_default();
        // A row persisted before TR-W1a (`touch_source` absent) never stamped its union: its
        // declared touch is still on its steps, so union from there (codex review on #693).
        let earlier: &[String] = match prior {
            Some(p) if p.touch_source.is_none() => p.steps.touch.as_deref().unwrap_or_default(),
            Some(p) => p.touch.as_slice(),
            None => &[],
        };
        for path in earlier.iter().chain(own) {
            if union.contains(path) {
                continue;
            }
            if union.len() == ACCEPTED_TOUCH_CAP {
                truncated = true;
                break;
            }
            union.push(path.clone());
        }
        self.touch_source = Some(if union.is_empty() {
            TouchSource::None
        } else if own.is_empty() {
            // An old row that declared a touch could not record who did: `user`, the source of
            // every declared touch before X1's PA scope.
            prior
                .and_then(|p| p.touch_source)
                .unwrap_or(TouchSource::User)
        } else {
            source
        });
        self.touch = union;
        self.touch_truncated = truncated;
        self
    }
}

/// The touch source of a proposal's own declared touch (TR-W1a): the PA's scope answer
/// (`Understand`) is `pa_scope`; a launch plan, a gate edit or a human edit is `user`.
pub(crate) fn touch_source_of(source: &ProposalSource) -> TouchSource {
    match source {
        ProposalSource::Understand { .. } => TouchSource::PaScope,
        _ => TouchSource::User,
    }
}

/// A built fact waiting for the run's path (its `bus_emit` event type and payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedFact {
    pub event_type: String,
    pub payload: Value,
}

/// (codex round 9 on #622) A plan-gate answer decided and proven — every synchronous check ran
/// on it (round 4) — but NOT applied: P1's required `gate.decided` comes first. It is held on the
/// pending fact ([`crate::domain::PendingTeamFact::staged`]) and applied only on that fact's
/// acknowledgement, or when the operator continues the run without team; a reject drops it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedRelease {
    /// The plan state the answer commits (the accepted rev, or the re-opened held plan).
    pub state: TeamPlanState,
    /// The facts that follow `gate.decided` (an edit's `plan.proposed` / `path.scored`, a
    /// refused edit's `plan.refused`): queued on the plan state when the answer applies, so they
    /// publish once, ahead of the next `plan.accepted` / `gate.opened`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub facts: Vec<QueuedFact>,
    /// An accepted edit: the proven def the run re-plans onto.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub def: Option<StagedDef>,
    /// A refused edit: the gate re-opens (a fresh gate) instead of releasing the run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub reopen: bool,
}

/// The proven def of an accepted edit, durable. [`crate::workflow::PhaseDef::catalog`] is
/// `serde(skip)` (no def FILE may claim a catalog id), so the engine keeps the ids it computed
/// beside the def and restores them — the def that runs is the def that was checked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StagedDef {
    pub def: WorkflowDef,
    pub catalogs: Vec<Option<String>>,
}

impl StagedDef {
    pub(crate) fn new(def: WorkflowDef) -> Self {
        let catalogs = def.phases.iter().map(|p| p.catalog.clone()).collect();
        StagedDef { def, catalogs }
    }

    pub(crate) fn into_def(self) -> WorkflowDef {
        let mut def = self.def;
        for (phase, catalog) in def.phases.iter_mut().zip(self.catalogs) {
            phase.catalog = catalog;
        }
        def
    }
}

/// Build facts as durable queued lines (the shape [`queue`] writes).
pub(crate) fn queued_facts(facts: &[TeamEvent]) -> anyhow::Result<Vec<QueuedFact>> {
    facts
        .iter()
        .map(|f| {
            Ok(QueuedFact {
                event_type: f.event_type().to_string(),
                payload: f.to_payload()?,
            })
        })
        .collect()
}

/// Queue `facts` on the plan state (they follow `path.started`).
pub(crate) fn queue(state: &mut TeamPlanState, facts: &[TeamEvent]) -> anyhow::Result<()> {
    for f in facts {
        state.queued.push(QueuedFact {
            event_type: f.event_type().to_string(),
            payload: f.to_payload()?,
        });
    }
    Ok(())
}

/// Take the queued facts, in order (re-parsed through the T1 contract).
pub(crate) fn take_queued(state: &mut TeamPlanState) -> Vec<TeamEvent> {
    std::mem::take(&mut state.queued)
        .into_iter()
        .filter_map(|q| TeamEvent::from_payload(&q.event_type, &q.payload).ok())
        .collect()
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
    /// (TR-W1a) Where the held plan's declared touch came from, carried to its acceptance.
    /// `None` on a row persisted before the field existed (read as `user` on approval).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub touch_source: Option<TouchSource>,
    /// (WT-C3) The testing rules the held plan was composed under, carried to its acceptance.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<crate::team::events::RuleOutcome>,
    /// (core#711) The step ids already dispatched or done when the plan was held, in the order
    /// they ran. `steps` stays the LOGICAL plan (catalog order, what is accepted); the gate's
    /// prompt shows the running order, as the def runs it. Empty on an initial plan.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done: Vec<String>,
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
        /// (DES-TEAMING-002 rev 15, ASK-K2b) The revision crosses INTO WORK: a `kind:"change"`
        /// that adds the first creator step to an accepted plan that had none.
        first_creator: bool,
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
    // (rev 15) Crossing into work is the row symmetric to crossing into high risk: approval in
    // EVERY mode under its own reason (manual mode included — the row, not the mode, is why the
    // gate opened; codex review of #738), and the more specific reason when the high-risk row
    // fires too — the gate payload still says `high_risk: true`. "Chat first": work never starts
    // from a conversation on the engine's say-so.
    if let PlanEvent::Revision {
        first_creator: true,
        ..
    } = event
    {
        return Ok(Some(ApprovalReason::FirstCreator));
    }
    if !auto {
        return Ok(Some(ApprovalReason::ManualMode));
    }
    Ok(match event {
        PlanEvent::Initial => high_risk.then_some(ApprovalReason::HighRisk),
        PlanEvent::Revision {
            previous_high_risk,
            approved_high_risk,
            ..
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

/// `plan` with the run's `deliver` step added (codex round 6 on #622): ONE source of the
/// deliver step. A delivering launch whose plan also authors a `deliver` step is refused, never
/// silently resolved to one of them.
///
/// (X-MIG M12, DES-W7-M12) The step is appended, except in a plan that pauses for CONSENT before
/// a write outside the run (a step gated `consent_before`, mcp-server's `install`): there the pull
/// request must exist when the consent card asks, so `deliver` goes before the consent chain. The
/// anchor is the consent step's dry run (its dependency, when that is a `run` step, which is where
/// the consent offer reads its plan, core#820), else the consent step itself. `deliver` takes the
/// anchor's `depends_on`, the anchor depends on `deliver`, and `deliver` sits right before it.
fn with_deliver(plan: &PlanSteps, deliver: Option<&PlanStep>) -> Result<PlanSteps, String> {
    let mut out = plan.clone();
    if let Some(d) = deliver {
        if let Some(own) = out.steps.iter().find(|s| s.catalog == "deliver") {
            return Err(format!(
                "the launch delivers (it carries its deliver step) and the plan authors its own \
                 deliver step `{}` — one source of the deliver step: drop one",
                own.id
            ));
        }
        match consent_anchor(&out.steps) {
            Some(at) => {
                let mut step = d.clone();
                step.depends_on = Some(out.steps[at].depends_on.clone().unwrap_or_default());
                out.steps[at].depends_on = Some(vec![step.id.clone()]);
                out.steps.insert(at, step);
            }
            None => out.steps.push(d.clone()),
        }
    }
    Ok(out)
}

/// The step a delivering run's `deliver` goes before (see [`with_deliver`]): the plan's final
/// `consent_before` step's dry run — its one dependency, when that is the `run` step right before
/// it — else the consent step itself; `None` for a plan that asks no consent, or whose consent is
/// not its last step.
fn consent_anchor(steps: &[PlanStep]) -> Option<usize> {
    let consent = steps
        .iter()
        .position(|s| matches!(s.gate, Some(crate::workflow::GateSpec::ConsentBefore)))?;
    // Only a TERMINAL consent chain (codex r1 on #866): the consent step is the plan's last, and
    // its dry run directly precedes it. Work after the chain must reach the pull request, so a
    // plan that goes on past its consent keeps the deliver step last.
    if consent + 1 != steps.len() {
        return None;
    }
    let dry_run = match steps[consent].depends_on.as_deref() {
        Some([dep])
            if consent > 0
                && &steps[consent - 1].id == dep
                && steps[consent - 1].catalog == "run" =>
        {
            Some(consent - 1)
        }
        _ => None,
    };
    Some(dry_run.unwrap_or(consent))
}

/// Every `deliver` step of a composed-to-be plan — authored or the launch's — passes the same
/// [`check_deliver_step`] (a step whose id is not `deliver` is compose's `deliver_id_reserved`).
fn check_deliver_steps(plan: &PlanSteps) -> Result<(), String> {
    plan.steps
        .iter()
        .filter(|s| s.catalog == "deliver" && s.id == "deliver")
        .try_for_each(check_deliver_step)
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
    /// The floor fill the verdict was reached on (`None` when refused before it):
    /// what `Core::preview_plan` reads back.
    pub filled: Option<crate::plan::FloorFilled>,
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
    rules: &dyn RuleSource,
    now: i64,
) -> anyhow::Result<Decided> {
    let auto = is_auto(human_confirm);
    let (plan, deliver_refusal) = match with_deliver(&proposal.plan, prior.deliver_step.as_ref()) {
        Ok(p) => (with_default_ids(&p), None),
        Err(why) => (with_default_ids(&proposal.plan), Some(why)),
    };
    // (X-MIG M9) The launch's declared deliverables ride every plan this pipeline decides — the
    // launch plan, the PA-scoped plan, a whole-plan edit at the initial gate (codex r1/r2 on #858)
    // — joined to its last creator step (idempotent); a plan with no creator step is refused with
    // its facts like any other refusal.
    let (plan, deliver_refusal) = match deliver_refusal {
        Some(why) => (plan, Some(why)),
        None => match with_deliverables(plan.clone(), &prior.deliverables) {
            Ok(p) => (p, None),
            Err(e) => (plan, Some(format!("{e:#}"))),
        },
    };
    let deliver = deliver_cmd(prior.deliver_step.as_ref());
    let proposal_id = ev::mint_proposal_id(run_id, &proposal.by, &proposal.source);
    let base_rev = (prior.accepted_rev > 0).then_some(prior.accepted_rev);
    let events = vec![
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
            filled: None,
        })
    };
    if let Some(why) = deliver_refusal {
        return refuse(events, why);
    }
    // §8.5 ratchet: the floor band is the maximum any score of the run has reached.
    let max_score = prior.max_score.max(scored.assessment.score);
    let destructive = prior.destructive || scored.destructive;
    // (WT-C3, DES-walkthrough-proof §4.12) The testing rules, read at compose against the
    // engine-derived context of this proposal.
    let context = plan_context(
        rules.projects(),
        plan.touch.as_deref().unwrap_or_default(),
        rules.teamed(),
        scored
            .assessment
            .signals
            .as_ref()
            .is_some_and(|s| s.critical),
        destructive,
        &crate::review_scale::floor_for(max_score, destructive).band,
        deliver.is_some(),
    );
    let read = rules.eval(&context)?;
    // The obligation ratchet: every held rule the run has fired, plus this proposal's.
    let obligations = union(&prior.obligations, &read.held);
    let recalled = union(&prior.recalled, &read.recalled);
    let filled = match crate::plan::floor_fill(
        crate::catalog::catalog(),
        &plan,
        crate::plan::FloorInput {
            score: max_score,
            destructive,
            human_confirm,
            deliver: deliver.as_deref(),
            obligations: &obligations,
            ran: &[],
        },
    ) {
        Ok(f) => f,
        Err(r) => return refuse(events, refusal_text(&r)),
    };
    let rules = rule_outcomes(&obligations, &recalled, &filled);
    if let Err(why) = check_deliver_steps(&filled.steps) {
        return refuse(events, why);
    }
    let rev = prior.rev + 1;
    let mut def = filled.def.clone();
    def.id = per_run_def_id(run_id, rev);
    let read_back = Some(crate::plan::FloorFilled {
        def: def.clone(),
        ..filled.clone()
    });
    // The run's first plan — or (X1) the launch plan its PA just scoped, whose only accepted rev
    // is the read-only scope step — meets the matrix's initial row.
    let event = if prior.accepted_rev == 0 || proposal.kind == ProposalKind::Initial {
        PlanEvent::Initial
    } else {
        PlanEvent::Revision {
            previous_high_risk: prior.accepted_high_risk,
            approved_high_risk: prior.approved_high_risk,
            first_creator: false,
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
    state.obligations = obligations;
    state.recalled = recalled;
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
            // `plan.accepted` is P1's required transition, published before the rev's first
            // dispatch from this record (`actor::team_gate`).
            state.accepted = Some(
                AcceptedPlan {
                    rev,
                    by: by.to_string(),
                    band: filled.band.clone(),
                    high_risk: filled.high_risk,
                    auto,
                    steps: filled.steps.clone(),
                    floor_override: filled.floor_override.clone(),
                    proposal_id: proposal_id.clone(),
                    touch: Vec::new(),
                    touch_truncated: false,
                    touch_source: None,
                    rules,
                }
                .with_touch(prior.accepted.as_ref(), touch_source_of(&proposal.source)),
            );
            state.accepted_rev = rev;
            state.accepted_high_risk = filled.high_risk;
            state.approved_high_risk |= proposal.approved_by_human && filled.high_risk;
            state.pending = None;
            Ok(Decided {
                state,
                events,
                verdict: Verdict::Accepted { def },
                filled: read_back,
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
                touch_source: Some(touch_source_of(&proposal.source)),
                rules,
                done: Vec::new(),
            });
            Ok(Decided {
                state,
                events,
                verdict: Verdict::Held { def },
                filled: read_back,
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
    deliverables: &[String],
) -> anyhow::Result<Option<(PlanSteps, Option<String>)>> {
    let resolved = resolve_launch_plan(store, plan, workflow, project_id)?;
    match resolved {
        Some((plan, preset)) => Ok(Some((with_deliverables(plan, deliverables)?, preset))),
        None if !deliverables.is_empty() => anyhow::bail!(
            "declared deliverables ride a plan or a preset launch — this launch names neither, so \
             they would be dropped"
        ),
        None => Ok(None),
    }
}

/// (X-MIG M9) `plan` with the launch's declared `deliverables` joined to the `required_deliverables`
/// of its LAST creator step (deduplicated, in order) — the engine's deliverable floor then judges
/// them on that step. Unchanged when there are none; refused when the plan has no creator step.
pub(crate) fn with_deliverables(
    mut plan: PlanSteps,
    deliverables: &[String],
) -> anyhow::Result<PlanSteps> {
    if deliverables.is_empty() {
        return Ok(plan);
    }
    if let Some(d) = deliverables.iter().find(|d| d.trim().is_empty()) {
        anyhow::bail!("a declared deliverable path must not be blank: {d:?}");
    }
    let catalog = crate::catalog::catalog();
    let Some(step) = plan
        .steps
        .iter_mut()
        .rev()
        .find(|s| crate::plan::is_creator_step(catalog, s))
    else {
        anyhow::bail!(
            "declared deliverables ride the plan's last creator step — this plan has none, so \
             nothing would write them"
        );
    };
    let list = step.required_deliverables.get_or_insert_with(Vec::new);
    for d in deliverables {
        if !list.contains(d) {
            list.push(d.clone());
        }
    }
    Ok(plan)
}

/// The launch's plan before its declared deliverables: a user plan, or the steps of the preset
/// `workflow` names.
fn resolve_launch_plan(
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

/// A launch plan refused before the run exists. No run exists, so no path carries a fact: the
/// refusal is the launch's synchronous error.
pub(crate) struct Refusal {
    pub reason: String,
}

/// The launch-time checks that do not depend on the score (§8.4-§8.5): supplied provenance, a
/// floor override in auto mode, and the authored steps composing over the catalog. A refusal is
/// a synchronous launch error with `plan.proposed` + `plan.refused` published; everything that
/// depends on the score is judged once the worktree's base commit is known ([`decide`]).
pub(crate) fn precheck(
    plan: &PlanSteps,
    deliver_step: Option<&PlanStep>,
    human_confirm: &HumanConfirm,
) -> Result<(), Refusal> {
    let (plan, one_source) = match with_deliver(plan, deliver_step) {
        Ok(p) => (with_default_ids(&p), None),
        Err(why) => (with_default_ids(plan), Some(why)),
    };
    let reason = if let Some(Err(why)) = deliver_step.map(check_deliver_step) {
        Some(why)
    } else if one_source.is_some() {
        one_source
    } else if let Some(s) = plan.steps.iter().find(|s| s.has_provenance()) {
        Some(refusal_text(&PlanRefusal::ProvenanceSupplied {
            step: s.id.clone(),
            catalog: s.catalog.clone(),
        }))
    } else if plan.floor_override.is_some() && is_auto(human_confirm) {
        Some(OVERRIDE_IN_AUTO.to_string())
    } else if plan.floor_override.is_some() && needs_pa_scope(&plan) {
        // (X1) The floor an override removes from is computed from the score, and a creator plan
        // with no touch set is scored by its PA after launch: nothing to judge the override by.
        Some(OVERRIDE_NEEDS_TOUCH.to_string())
    } else {
        crate::plan::compose(crate::catalog::catalog(), &plan)
            .err()
            .map(|r| refusal_text(&r))
            .or_else(|| check_deliver_steps(&plan).err())
    };
    match reason {
        Some(reason) => Err(Refusal { reason }),
        None => Ok(()),
    }
}

/// The phase types floor fill could add to a launch plan in the worst case (the plan with its
/// deliver step, as it will be floor-filled): the launch's synchronous unit limit counts them.
pub(crate) fn worst_case_floor_additions(
    plan: &PlanSteps,
    deliver_step: Option<&PlanStep>,
) -> Vec<String> {
    // A plan the launch refuses for two deliver sources is counted as authored (it never runs).
    let plan = with_default_ids(&with_deliver(plan, deliver_step).unwrap_or_else(|_| plan.clone()));
    crate::plan::worst_case_floor_additions(
        crate::catalog::catalog(),
        &plan,
        deliver_step.is_some(),
    )
}

/// The launch plan's AUTHORED steps composed as rev 1 (`<run>:plan-1`), for the launch-time
/// checks that read a def (tool preflight, base skill, seat need). The floor is added later. A
/// plan the PA scopes (X1) is checked with its scope step first, as it will run (`unbound`: the
/// run has no repo).
pub(crate) fn authored_def(
    run_id: &str,
    plan: &PlanSteps,
    deliver_step: Option<&PlanStep>,
    unbound: bool,
) -> anyhow::Result<WorkflowDef> {
    let plan = with_scope_step(plan, unbound);
    let plan = with_default_ids(&with_deliver(&plan, deliver_step).map_err(anyhow::Error::msg)?);
    let mut def = crate::plan::compose(crate::catalog::catalog(), &plan)
        .map_err(|r| anyhow::anyhow!("{r}"))?;
    def.id = per_run_def_id(run_id, 1);
    Ok(def)
}

/// Approve the held plan as-is at its gate (§8.6): the gate's `gate.decided{human_approved}` (P1
/// required: it gates the resume) and the state with the rev accepted — its `plan.accepted{by:
/// "human"}` follows before the first dispatch.
pub(crate) fn approve_pending(
    run_id: &str,
    state: &TeamPlanState,
    human_confirm: &HumanConfirm,
    ord: u32,
    attempt: u32,
    now: i64,
) -> anyhow::Result<(TeamPlanState, TeamEvent)> {
    let p = state
        .pending
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("run {run_id} holds no plan for approval"))?;
    let gate_id = p
        .gate_id
        .clone()
        .ok_or_else(|| anyhow::anyhow!("run {run_id}'s plan gate never opened"))?;
    let decided = gate_decided(
        run_id,
        &gate_id,
        ord,
        attempt,
        GateDecision::HumanApproved,
        now,
    )?;
    let mut next = state.clone();
    next.accepted = Some(
        AcceptedPlan {
            rev: p.rev,
            by: "human".to_string(),
            band: p.band.clone(),
            high_risk: p.high_risk,
            auto: is_auto(human_confirm),
            steps: p.steps.clone(),
            floor_override: p.floor_override.clone(),
            proposal_id: p.proposal_id.clone(),
            touch: Vec::new(),
            touch_truncated: false,
            touch_source: None,
            rules: p.rules.clone(),
        }
        .with_touch(
            state.accepted.as_ref(),
            p.touch_source.unwrap_or(TouchSource::User),
        ),
    );
    next.accepted_rev = p.rev;
    next.accepted_high_risk = p.high_risk;
    next.approved_high_risk |= p.high_risk;
    next.pending = None;
    Ok((next, decided))
}

/// The prompt a `plan_approval` gate shows (§8.5: the override is shown at the gate). The steps
/// are listed in RUNNING order (core#711): the done prefix as it ran, then the rest of the
/// logical plan — a floor step added after `build` finished shows after `build`, where it runs,
/// as the studio stepper shows it. A done step the plan dropped falls back to the logical order.
pub(crate) fn gate_prompt(p: &PendingPlan, ord: u32, auto: bool) -> String {
    let running = revise::running_order(&p.steps, &p.done).unwrap_or_else(|_| p.steps.clone());
    let steps: Vec<String> = running
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
    if let Some(b) = s.budget_secs {
        o.insert("budget_secs".into(), json!(b));
    }
    if let Some(p) = s.pool {
        o.insert("pool".into(), json!(p));
    }
    if let Some(a) = &s.added_by {
        o.insert("added_by".into(), json!(a));
    }
    if let Some(r) = &s.floor_reason {
        o.insert("floor_reason".into(), json!(r));
    }
    if let Some(r) = &s.floor_rule {
        o.insert("floor_rule".into(), json!(r));
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
            // (ASK-K1b) The plan's own ask; the supervisor ratchets it, never lowers it.
            "monitors": plan
                .monitors
                .clone()
                .unwrap_or(crate::team::events::MonitorsAsk { asked: 0 }),
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
            "critical": s.critical, "destructive": s.destructive, "truncated": s.truncated(),
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

/// `plan.accepted` for an accepted rev (§6 row 5): the composed steps with their provenance, the
/// band, `high_risk`, the mode and the recorded override — all engine-computed.
pub(crate) fn plan_accepted(run_id: &str, a: &AcceptedPlan, now: i64) -> anyhow::Result<TeamEvent> {
    let mut body = json!({
            "plan_rev": a.rev,
            "workflow_id": per_run_def_id(run_id, a.rev),
            "band": a.band,
            "high_risk": a.high_risk,
            "mode": if a.auto { "auto" } else { "manual" },
            "steps": wire_steps(&a.steps),
            "override": a.floor_override,
            // A floor raise composes no proposal (§6 row 4): `null`, never an empty id.
            "proposal_id": (!a.proposal_id.is_empty()).then_some(&a.proposal_id),
            // (TR-W1a) Always named on a new row; an absent key is an old engine.
            "touch_source": a.touch_source.unwrap_or(TouchSource::None),
    });
    // (TR-W1a) Omitted when empty / not cut, exactly as the wire struct skips them.
    if !a.touch.is_empty() {
        body["touch"] = json!(a.touch);
    }
    if a.touch_truncated {
        body["touch_truncated"] = json!(true);
    }
    // (WT-C3) Omitted when no rule applied, exactly as the wire struct skips it.
    if !a.rules.is_empty() {
        body["rules"] = json!(a.rules);
    }
    build(
        ev::PLAN_ACCEPTED,
        envelope(run_id, &a.by, None, None, now),
        body,
    )
}

/// (T8 (c)) The record of human edits refused AFTER `Core::propose_plan` took them (the deliver
/// step started, or the revision could not be planned): each edit's
/// `plan.proposed{by:"human", kind:"edit"}` and its `plan.refused`, so no edit is silently spent.
pub(crate) fn edits_refused(
    run_id: &str,
    base_rev: Option<u32>,
    edits: &[HeldEdit],
    reason: &str,
    now: i64,
) -> anyhow::Result<Vec<TeamEvent>> {
    let mut out = Vec::new();
    for e in edits {
        let pid = ev::mint_proposal_id(
            run_id,
            "human",
            &ProposalSource::Edit {
                request_id: e.request_id.clone(),
            },
        );
        let plan = PlanSteps {
            steps: e.steps.clone(),
            monitors: None,
            touch: None,
            floor_override: None,
        };
        out.push(plan_proposed(
            run_id,
            "human",
            &pid,
            base_rev,
            ProposalKind::Edit,
            None,
            &plan,
            now,
        )?);
        out.push(plan_refused(run_id, &pid, base_rev, reason, now)?);
    }
    Ok(out)
}

pub(crate) fn plan_refused(
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

#[cfg(test)]
mod tests {
    use super::*;

    // (X-MIG M12) A delivering run's deliver step goes before the consent chain.
    fn ws(id: &str, catalog: &str, deps: &[&str]) -> PlanStep {
        PlanStep {
            catalog: catalog.into(),
            id: id.into(),
            depends_on: Some(deps.iter().map(|d| d.to_string()).collect()),
            ..PlanStep::default()
        }
    }

    fn deliver_step_for_test() -> PlanStep {
        PlanStep {
            catalog: "deliver".into(),
            id: "deliver".into(),
            executor: Some(crate::workflow::PhaseExecutor::Tool {
                cmd: vec!["true".into()],
            }),
            ..PlanStep::default()
        }
    }

    #[test]
    fn m12_deliver_goes_before_the_consent_steps_dry_run() {
        let mut install = ws("install", "run", &["install-plan"]);
        install.gate = Some(crate::workflow::GateSpec::ConsentBefore);
        let plan = PlanSteps {
            steps: vec![
                ws("build", "build", &[]),
                ws("review", "review", &["build"]),
                ws("install-plan", "run", &["review"]),
                install,
            ],
            ..Default::default()
        };
        let out = with_deliver(&plan, Some(&deliver_step_for_test())).unwrap();
        let ids: Vec<_> = out.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            ["build", "review", "deliver", "install-plan", "install"]
        );
        let deps = |id: &str| {
            out.steps
                .iter()
                .find(|s| s.id == id)
                .unwrap()
                .depends_on
                .clone()
        };
        assert_eq!(deps("deliver"), Some(vec!["review".to_string()]));
        assert_eq!(deps("install-plan"), Some(vec!["deliver".to_string()]));
        assert_eq!(deps("install"), Some(vec!["install-plan".to_string()]));
    }

    #[test]
    fn m12_deliver_goes_before_a_consent_step_with_no_dry_run_and_is_appended_without_one() {
        let mut install = ws("install", "run", &["review"]);
        install.gate = Some(crate::workflow::GateSpec::ConsentBefore);
        let plan = PlanSteps {
            steps: vec![ws("review", "review", &[]), install],
            ..Default::default()
        };
        let out = with_deliver(&plan, Some(&deliver_step_for_test())).unwrap();
        let ids: Vec<_> = out.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["review", "deliver", "install"]);
        let plain = PlanSteps {
            steps: vec![
                ws("build", "build", &[]),
                ws("review", "review", &["build"]),
            ],
            ..Default::default()
        };
        let out = with_deliver(&plain, Some(&deliver_step_for_test())).unwrap();
        assert_eq!(out.steps.last().unwrap().id, "deliver");
    }

    #[test]
    fn m12_a_consent_chain_with_work_after_it_keeps_deliver_last() {
        // codex r1 on #866: work after the chain must reach the pull request.
        let mut install = ws("install", "run", &["install-plan"]);
        install.gate = Some(crate::workflow::GateSpec::ConsentBefore);
        let plan = PlanSteps {
            steps: vec![
                ws("build", "build", &[]),
                ws("install-plan", "run", &["build"]),
                install,
                ws("build-2", "build", &["install"]),
                ws("review-2", "review", &["build-2"]),
            ],
            ..Default::default()
        };
        let out = with_deliver(&plan, Some(&deliver_step_for_test())).unwrap();
        let ids: Vec<_> = out.steps.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "build",
                "install-plan",
                "install",
                "build-2",
                "review-2",
                "deliver"
            ]
        );
    }
    use crate::review_scale::Graph;
    use crate::team::events::{TeamBody, PATH_SCORED, PLAN_ACCEPTED, PLAN_PROPOSED};
    use serde_json::json;

    fn plan(v: serde_json::Value) -> PlanSteps {
        serde_json::from_value(v).unwrap()
    }

    /// (X-MIG M9) A launch's declared deliverables join the LAST creator step's
    /// `required_deliverables` (deduplicated, after any the step already declares); a plan with no
    /// creator step, a blank path, or a launch with neither a plan nor a preset is refused.
    #[test]
    fn declared_deliverables_join_the_last_creator_step() {
        let plan: PlanSteps = serde_json::from_value(serde_json::json!({"steps": [
            {"catalog": "understand", "id": "outline"},
            {"catalog": "produce", "id": "first"},
            {"catalog": "produce", "id": "draft", "required_deliverables": ["a.md"]},
            {"catalog": "critique", "id": "read"}
        ]}))
        .unwrap();
        let d = vec!["/w/out.html".to_string(), "a.md".to_string()];
        let out = with_deliverables(plan.clone(), &d).unwrap();
        let by = |id: &str| out.steps.iter().find(|s| s.id == id).unwrap();
        assert_eq!(
            by("draft").required_deliverables.as_deref(),
            Some(&["a.md".to_string(), "/w/out.html".to_string()][..])
        );
        assert_eq!(by("first").required_deliverables, None);
        assert_eq!(with_deliverables(plan.clone(), &[]).unwrap(), plan);
        assert!(with_deliverables(plan, &["  ".to_string()]).is_err());
        let read_only: PlanSteps =
            serde_json::from_value(serde_json::json!({"steps": [{"catalog": "understand"}]}))
                .unwrap();
        let e = with_deliverables(read_only, &d).unwrap_err().to_string();
        assert!(
            e.contains("no creator step") || e.contains("has none"),
            "{e}"
        );
        let store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let e = launch_plan(&store, None, None, None, &d)
            .unwrap_err()
            .to_string();
        assert!(e.contains("names neither"), "{e}");
        assert!(launch_plan(&store, None, None, None, &[])
            .unwrap()
            .is_none());
    }

    /// (DES-ASK-TEAM-CHAT-001 §4.6, ASK-K1b) The plan's `monitors.asked` rides `plan.proposed`
    /// (T2 §6.1's payload); a plan without one says 0, byte-identical to before.
    #[test]
    fn plan_proposed_carries_the_plans_monitor_ask() {
        let asked = plan(json!({"steps": [{"catalog": "understand"}], "monitors": {"asked": 1}}));
        let ev = plan_proposed(
            "r1",
            "human",
            "p-1",
            None,
            ProposalKind::Initial,
            None,
            &asked,
            7,
        )
        .unwrap();
        assert_eq!(ev.to_payload().unwrap()["monitors"], json!({"asked": 1}));
        let bare = plan(json!({"steps": [{"catalog": "understand"}]}));
        let ev = plan_proposed(
            "r1",
            "human",
            "p-2",
            None,
            ProposalKind::Initial,
            None,
            &bare,
            7,
        )
        .unwrap();
        assert_eq!(ev.to_payload().unwrap()["monitors"], json!({"asked": 0}));
    }

    /// (core#711) The gate prompt lists the steps as they RUN, not as the logical plan orders
    /// them: a floor step added after `build` finished shows after `build`. An initial plan (no
    /// done prefix) and a plan whose done step is missing fall back to the logical order.
    #[test]
    fn the_gate_prompt_lists_steps_in_running_order() {
        let steps = plan(json!({"steps": [
            {"catalog": "understand", "id": "pa-scope", "added_by": "plan"},
            {"catalog": "test_plan", "id": "test_plan", "added_by": "floor"},
            {"catalog": "design", "id": "design", "added_by": "floor"},
            {"catalog": "build", "id": "build", "added_by": "plan"},
            {"catalog": "test", "id": "test", "added_by": "plan"},
            {"catalog": "review", "id": "review", "added_by": "plan"},
            {"catalog": "deliver", "id": "deliver", "added_by": "plan"}
        ]}));
        let mut p = PendingPlan {
            rev: 3,
            proposal_id: "p-rescore".into(),
            reviewing_ord: Some(2),
            steps,
            band: "40-69".into(),
            high_risk: false,
            floor_override: None,
            reason: "manual_mode".into(),
            floor_added: vec!["test_plan".into(), "design".into()],
            gate_id: None,
            refusal: None,
            touch_source: None,
            rules: Vec::new(),
            done: vec!["pa-scope".into(), "build".into()],
        };
        let prompt = gate_prompt(&p, 3, false);
        assert!(
            prompt.contains(
                "pa-scope → build → test_plan (floor) → design (floor) → test → review → deliver"
            ),
            "{prompt}"
        );
        assert!(
            prompt.contains("Floor added: test_plan, design"),
            "{prompt}"
        );

        let logical =
            "pa-scope → test_plan (floor) → design (floor) → build → test → review → deliver";
        p.done = Vec::new();
        assert!(gate_prompt(&p, 1, false).contains(logical));
        p.done = vec!["gone".into()];
        assert!(gate_prompt(&p, 1, false).contains(logical));
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
            first_creator: false,
        };
        assert_eq!(approval(false, true, false, into), Ok(Some(ManualMode)));
        assert_eq!(approval(true, true, false, into), Ok(Some(IntoHighRisk)));
        // Revision that STAYS high risk after an approved high-risk rev: manual approval, auto proceeds.
        let stays = PlanEvent::Revision {
            previous_high_risk: true,
            approved_high_risk: true,
            first_creator: false,
        };
        assert_eq!(approval(false, true, false, stays), Ok(Some(ManualMode)));
        assert_eq!(approval(true, true, false, stays), Ok(None));
        // …but high risk that was never approved still needs it (positive evidence only).
        let never = PlanEvent::Revision {
            previous_high_risk: true,
            approved_high_risk: false,
            first_creator: false,
        };
        assert_eq!(approval(true, true, false, never), Ok(Some(HighRisk)));
        // Any other revision: manual approval, auto proceeds.
        let other = PlanEvent::Revision {
            previous_high_risk: false,
            approved_high_risk: false,
            first_creator: false,
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

    /// WT-C3: a rule source that answers with fixed rules and records every context it was
    /// asked about.
    struct Fixed {
        rules: RulesEval,
        projects: Vec<String>,
        teamed: bool,
        asked: std::cell::RefCell<Vec<Value>>,
    }

    impl Fixed {
        fn new(rules: RulesEval, teamed: bool) -> Self {
            Fixed {
                rules,
                projects: vec!["proj-b".into(), "proj-a".into()],
                teamed,
                asked: Default::default(),
            }
        }
    }

    impl RuleSource for Fixed {
        fn projects(&self) -> &[String] {
            &self.projects
        }
        fn teamed(&self) -> bool {
            self.teamed
        }
        fn eval(&self, context: &Value) -> anyhow::Result<RulesEval> {
            self.asked.borrow_mut().push(context.clone());
            Ok(self.rules.clone())
        }
    }

    fn walkthrough_rule() -> RulesEval {
        RulesEval {
            held: vec![HeldObligation {
                rule: "TST-1002".into(),
                token: "step:walkthrough".into(),
            }],
            recalled: vec!["TST-1001".into()],
        }
    }

    fn launch_decide(
        p: &PlanSteps,
        prior: &TeamPlanState,
        hc: &HumanConfirm,
        rules: &dyn RuleSource,
    ) -> Decided {
        decide(
            "r1",
            Proposal {
                by: "human".into(),
                source: ProposalSource::Launch {
                    session_id: "r1".into(),
                },
                kind: ProposalKind::Initial,
                preset: None,
                plan: p.clone(),
                reviewing_ord: None,
                approved_by_human: false,
            },
            prior,
            hc,
            &scored_for(p),
            rules,
            1,
        )
        .unwrap()
    }

    /// WT-C3 (§4.12 B5): the rules are read against the ENGINE-derived context — `kinds` from
    /// the declared touch on a teamed run, failing closed to `["code"]` on a run not (yet) teamed
    /// or with no touch; the band the ratcheted score lands in; the run's first project.
    #[test]
    fn wt_c3_rules_are_read_against_the_derived_plan_context() {
        let p = plan(json!({"steps": [{"catalog": "build"}], "touch": ["README.md", "docs/x.md"]}));
        let teamed = Fixed::new(RulesEval::default(), true);
        launch_decide(&p, &TeamPlanState::default(), &HumanConfirm::None, &teamed);
        let ctx = teamed.asked.borrow()[0].clone();
        assert_eq!(
            ctx,
            json!({"project": "proj-a", "kinds": ["docs"], "paths": ["README.md", "docs/x.md"],
                   "critical": false, "destructive": false, "band": "0-19", "deliver": false})
        );
        let unteamed = Fixed::new(RulesEval::default(), false);
        launch_decide(
            &p,
            &TeamPlanState::default(),
            &HumanConfirm::None,
            &unteamed,
        );
        assert_eq!(unteamed.asked.borrow()[0]["kinds"], json!(["code"]));
        let no_touch = plan(json!({"steps": [{"catalog": "understand"}], "touch": []}));
        launch_decide(
            &no_touch,
            &TeamPlanState::default(),
            &HumanConfirm::None,
            &teamed,
        );
        assert_eq!(teamed.asked.borrow()[1]["kinds"], json!(["code"]));
        // A kinds-mixed touch is the sorted set.
        let mixed = plan_context(
            &[],
            &[
                "src/a.rs".into(),
                "a.json".into(),
                "tests/t.rs".into(),
                "README.md".into(),
            ],
            true,
            false,
            false,
            "40-69",
            true,
        );
        assert_eq!(mixed["kinds"], json!(["code", "config", "docs", "test"]));
        assert_eq!(mixed["project"], Value::Null);
        assert_eq!(mixed["deliver"], true);
    }

    /// WT-C3: a fired held rule's pair is in the accepted plan with its `floor_rule`, the
    /// obligation is ratcheted onto the run's state, and `plan.accepted.rules` records it
    /// `applied` beside the advisory rule `recalled`. A later rev keeps the obligation even when
    /// its own context fires nothing.
    #[test]
    fn wt_c3_a_held_rule_is_applied_recorded_and_ratcheted() {
        let p = plan(json!({"steps": [{"catalog": "build"}], "touch": ["README.md"]}));
        let src = Fixed::new(walkthrough_rule(), true);
        let d = launch_decide(&p, &TeamPlanState::default(), &HumanConfirm::None, &src);
        assert!(matches!(d.verdict, Verdict::Accepted { .. }));
        let acc = d.state.accepted.clone().unwrap();
        let wt: Vec<(&str, Option<&str>)> = acc
            .steps
            .steps
            .iter()
            .filter(|s| s.catalog.starts_with("walkthrough"))
            .map(|s| (s.catalog.as_str(), s.floor_rule.as_deref()))
            .collect();
        assert_eq!(
            wt,
            [
                ("walkthrough_plan", Some("TST-1002")),
                ("walkthrough_review", Some("TST-1002"))
            ]
        );
        let payload = plan_accepted("r1", &acc, 1).unwrap().to_payload().unwrap();
        assert_eq!(
            payload["rules"],
            json!([{"id": "TST-1001", "outcome": "recalled"},
                   {"id": "TST-1002", "outcome": "applied"}])
        );
        let wire_wp = payload["steps"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["catalog"] == "walkthrough_plan")
            .unwrap()
            .clone();
        assert_eq!(wire_wp["floor_rule"], "TST-1002");
        assert_eq!(
            wire_wp["floor_reason"],
            "rule TST-1002 requires walkthrough_plan"
        );
        // The typed wire reads it back.
        let ev = plan_accepted("r1", &acc, 1).unwrap();
        let TeamBody::PlanAccepted(body) = &ev.body else {
            panic!("plan.accepted")
        };
        assert_eq!(body.rules.len(), 2);
        // The ratchet: the next rev fires nothing and still owes the pair.
        let none = Fixed::new(RulesEval::default(), true);
        let d2 = launch_decide(&p, &d.state, &HumanConfirm::None, &none);
        let acc2 = d2.state.accepted.unwrap();
        assert!(acc2
            .steps
            .steps
            .iter()
            .any(|s| s.catalog == "walkthrough_review"));
        assert_eq!(d2.state.obligations, d.state.obligations);
        assert_eq!(acc2.rules.len(), 2);
    }

    /// WT-C3: no rule ⇒ the wire is unchanged (`plan.accepted` has no `rules` key, no step a
    /// `floor_rule`).
    #[test]
    fn wt_c3_without_rules_plan_accepted_is_unchanged() {
        let p = plan(json!({"steps": [{"catalog": "build"}], "touch": ["README.md"]}));
        let d = launch_decide(&p, &TeamPlanState::default(), &HumanConfirm::None, &NoRules);
        let payload = plan_accepted("r1", &d.state.accepted.unwrap(), 1)
            .unwrap()
            .to_payload()
            .unwrap();
        assert!(payload.get("rules").is_none(), "{payload}");
        assert!(!payload.to_string().contains("floor_rule"), "{payload}");
    }

    /// WT-C3: a rule that denies at compose refuses the plan, naming it; an override that
    /// removes the rule's whole pair (manual mode) records it `overridden`.
    #[test]
    fn wt_c3_a_denying_rule_refuses_and_an_overridden_rule_is_recorded() {
        let p = plan(json!({"steps": [{"catalog": "build"}], "touch": ["README.md"]}));
        // A fired deny (or allow) row at plan.compose — refused at write, so only a row written
        // around the check — fails closed: the plan is refused naming the rule.
        let policy = |id: &str, effect| wicked_governance::Policy {
            id: id.into(),
            kind: "testing".into(),
            applies_to: vec![wicked_governance::PLAN_COMPOSE_PHASE.into()],
            effect,
            trigger: Default::default(),
            obligations: Vec::new(),
            criteria: String::new(),
            severity: wicked_governance::Severity::Medium,
            rule: String::new(),
            retired: false,
        };
        for (id, effect) in [
            ("TST-DENY", wicked_governance::Effect::Deny),
            ("TST-ALLOW", wicked_governance::Effect::Allow),
            ("TST-EMPTY", wicked_governance::Effect::AllowWithConditions),
        ] {
            let read = RulesEval::from_phase_rules(&wicked_governance::PhaseRules {
                fired: vec![policy(id, effect)],
                recalled: Vec::new(),
            });
            let src = Fixed::new(read, true);
            let d = launch_decide(&p, &TeamPlanState::default(), &HumanConfirm::None, &src);
            assert!(
                body_types(&d).contains(&ev::PLAN_REFUSED),
                "{:?}",
                body_types(&d)
            );
            let Verdict::Refused { reason } = d.verdict else {
                panic!("{id}: refused")
            };
            assert!(
                reason.contains(id) && reason.contains("unknown_obligation"),
                "{reason}"
            );
        }
        let over = plan(
            json!({"steps": [{"catalog": "build"}], "touch": ["README.md"],
            "override": {"remove": ["walkthrough_plan", "walkthrough_review"], "reason": "mine"}}),
        );
        let src = Fixed::new(walkthrough_rule(), true);
        let d = launch_decide(&over, &TeamPlanState::default(), &HumanConfirm::All, &src);
        let pending = d.state.pending.expect("a manual-mode plan is held");
        assert_eq!(
            pending.rules,
            [
                crate::team::events::RuleOutcome {
                    id: "TST-1001".into(),
                    outcome: crate::team::events::RuleOutcomeKind::Recalled,
                },
                crate::team::events::RuleOutcome {
                    id: "TST-1002".into(),
                    outcome: crate::team::events::RuleOutcomeKind::Overridden,
                },
            ]
        );
        assert!(!pending
            .steps
            .steps
            .iter()
            .any(|s| s.catalog.starts_with("walkthrough")));
        // Removing ONE half disables the pair too: the rule reads `overridden` (Copilot review).
        let half = plan(
            json!({"steps": [{"catalog": "build"}], "touch": ["README.md"],
            "override": {"remove": ["walkthrough_review"], "reason": "mine"}}),
        );
        let d = launch_decide(&half, &TeamPlanState::default(), &HumanConfirm::All, &src);
        let rules = d.state.pending.expect("held").rules;
        assert_eq!(
            rules[1].outcome,
            crate::team::events::RuleOutcomeKind::Overridden
        );
    }

    fn body_types(d: &Decided) -> Vec<&'static str> {
        d.events.iter().map(TeamEvent::event_type).collect()
    }

    /// TR-W1a (DES-trigger-registry §4.9): `plan.accepted` carries the accepted rev's declared
    /// touch set and where it came from — a launch plan's own touch is `user`, a PA scope
    /// answer's is `pa_scope`, no declared scope is `none` — unioned with every earlier accepted
    /// rev's touch and capped at 64 paths with `touch_truncated`.
    #[test]
    fn plan_accepted_carries_the_accepted_touch_and_its_source() {
        let accept = |by: &str, source: ProposalSource, p: PlanSteps, prior: &TeamPlanState| {
            let d = decide(
                "r1",
                Proposal {
                    by: by.into(),
                    source,
                    kind: ProposalKind::Initial,
                    preset: None,
                    plan: p.clone(),
                    reviewing_ord: None,
                    approved_by_human: false,
                },
                prior,
                &HumanConfirm::None,
                &scored_for(&p),
                &crate::plan_gate::NoRules,
                1,
            )
            .unwrap();
            assert!(matches!(d.verdict, Verdict::Accepted { .. }));
            let acc = d.state.accepted.clone().expect("accepted");
            let payload = plan_accepted("r1", &acc, 1).unwrap().to_payload().unwrap();
            (d.state, payload)
        };
        let launch = || ProposalSource::Launch {
            session_id: "r1".into(),
        };
        let read_only = |touch: serde_json::Value| {
            plan(json!({"steps": [{"catalog": "understand", "id": "u"}], "touch": touch}))
        };

        // A launch plan's own declared touch: `user`.
        let (state, p) = accept(
            "human",
            launch(),
            read_only(json!(["src/a.rs", "docs/"])),
            &TeamPlanState::default(),
        );
        assert_eq!(p["touch"], json!(["src/a.rs", "docs/"]));
        assert_eq!(p["touch_source"], "user");
        assert!(p.get("touch_truncated").is_none(), "{p}");

        // A later accepted rev unions the earlier rev's touch (order kept, no repeats).
        let (_, p) = accept(
            "human",
            launch(),
            read_only(json!(["docs/", "src/b.rs"])),
            &state,
        );
        assert_eq!(p["touch"], json!(["src/a.rs", "docs/", "src/b.rs"]));
        assert_eq!(p["touch_source"], "user");

        // The PA's scope answer: `pa_scope`.
        let (_, p) = accept(
            "claude#1",
            ProposalSource::Understand { ord: 1, attempt: 0 },
            read_only(json!(["src/c.rs"])),
            &TeamPlanState::default(),
        );
        assert_eq!(p["touch_source"], "pa_scope");
        assert_eq!(p["touch"], json!(["src/c.rs"]));

        // No declared scope: `none`, and no touch key.
        let (_, p) = accept(
            "human",
            launch(),
            plan(json!({"steps": [{"catalog": "understand", "id": "u"}]})),
            &TeamPlanState::default(),
        );
        assert_eq!(p["touch_source"], "none");
        assert!(p.get("touch").is_none(), "{p}");

        // Capped at 64 paths, flagged.
        let many: Vec<String> = (0..70).map(|i| format!("src/f{i}.rs")).collect();
        let (_, p) = accept(
            "human",
            launch(),
            read_only(json!(many)),
            &TeamPlanState::default(),
        );
        assert_eq!(p["touch"].as_array().unwrap().len(), 64);
        assert_eq!(p["touch"][63], "src/f63.rs");
        assert_eq!(p["touch_truncated"], true);
    }

    /// TR-W1a (codex review on #693): an accepted rev persisted before the touch fields existed
    /// still contributes its declared touch to the next rev's union (read off its steps).
    #[test]
    fn a_pre_w1a_accepted_row_still_unions_its_declared_touch() {
        let legacy: AcceptedPlan = serde_json::from_value(json!({
            "rev": 1, "by": "engine", "band": "0-19", "high_risk": false, "auto": true,
            "steps": {"steps": [{"catalog": "understand", "id": "u"}], "touch": ["src/a.rs"]},
            "floor_override": null, "proposal_id": "p-old"
        }))
        .unwrap();
        assert!(legacy.touch.is_empty() && legacy.touch_source.is_none());
        let next = AcceptedPlan {
            rev: 2,
            steps: plan(
                json!({"steps": [{"catalog": "understand", "id": "u"}], "touch": ["src/b.rs"]}),
            ),
            ..legacy.clone()
        }
        .with_touch(Some(&legacy), TouchSource::User);
        assert_eq!(next.touch, ["src/a.rs", "src/b.rs"]);
        assert_eq!(next.touch_source, Some(TouchSource::User));
        let same = AcceptedPlan {
            rev: 2,
            steps: plan(json!({"steps": [{"catalog": "understand", "id": "u"}]})),
            ..legacy.clone()
        }
        .with_touch(Some(&legacy), TouchSource::User);
        assert_eq!(same.touch, ["src/a.rs"]);
        assert_eq!(same.touch_source, Some(TouchSource::User));
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
                &crate::plan_gate::NoRules,
                1,
            )
            .unwrap();
            assert!(matches!(d.verdict, Verdict::Accepted { .. }), "{by}");
            // plan.accepted is P1's required transition: built from the accepted record.
            assert_eq!(body_types(&d), [PLAN_PROPOSED, PATH_SCORED], "{by}");
            let acc = d.state.accepted.as_ref().expect("accepted");
            let ev = plan_accepted("r1", acc, 1).unwrap();
            let TeamBody::PlanAccepted(a) = &ev.body else {
                panic!()
            };
            assert_eq!(
                (ev.env.by.as_str(), a.plan_rev, a.high_risk, a.band.as_str()),
                ("engine", 1, false, "0-19")
            );
            assert_eq!(a.workflow_id, "r1:plan-1");
            assert_eq!(ev.event_type(), PLAN_ACCEPTED);
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
                &crate::plan_gate::NoRules,
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
                &crate::plan_gate::NoRules,
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
            &crate::plan_gate::NoRules,
            1,
        )
        .unwrap();
        assert!(matches!(d.verdict, Verdict::Held { .. }));
        assert!(d.state.pending.unwrap().high_risk);
    }

    /// Facts queued before the run's path come back in order, through the T1 contract.
    #[test]
    fn queued_facts_round_trip_in_order() {
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
            &crate::plan_gate::NoRules,
            1,
        )
        .unwrap();
        let mut state = d.state.clone();
        queue(&mut state, &d.events).unwrap();
        let back = take_queued(&mut state);
        assert_eq!(back, d.events);
        assert!(state.queued.is_empty());
    }

    /// The worst-case floor additions: the top band's floor types the plan lacks (fixed from
    /// §8.5's 70-100 row), with the non-code substitution and `deliver` only for a delivering run.
    #[test]
    fn worst_case_floor_additions_are_the_top_band_types_the_plan_lacks() {
        let adds =
            |v: serde_json::Value, d: Option<&PlanStep>| worst_case_floor_additions(&plan(v), d);
        assert_eq!(
            adds(json!({"steps": [{"catalog": "build"}]}), None),
            [
                "test_plan",
                "design",
                "architecture",
                "review",
                "security_review"
            ]
        );
        assert_eq!(
            adds(
                json!({"steps": [{"catalog": "build"}, {"catalog": "review"}]}),
                None
            ),
            ["test_plan", "design", "architecture", "security_review"]
        );
        // No creator step: an empty floor.
        assert!(adds(json!({"steps": [{"catalog": "understand"}]}), None).is_empty());
        // A non-code run: `critique` fills the review slot, and no diff-floored
        // `security_review` (core#649).
        assert_eq!(
            adds(json!({"steps": [{"catalog": "produce"}]}), None),
            ["test_plan", "design", "architecture", "critique"]
        );
        // A delivering run carries its deliver step, so `deliver` is never an addition.
        let d: PlanStep = serde_json::from_value(json!({
            "catalog": "deliver", "id": "deliver", "executor": {"type": "tool", "cmd": ["true"]}
        }))
        .unwrap();
        assert_eq!(
            adds(json!({"steps": [{"catalog": "build"}]}), Some(&d)).len(),
            5
        );
    }

    /// WT-C4 (DES-walkthrough-proof §4.12 S5): the shipped testing starter, read by the engine
    /// against the contexts it derives itself. Advisory as shipped: TST-1001 for a docs-only
    /// touch, TST-1002 for code or config, TST-1003 for a risk path (a path word, split on `/`,
    /// `_`, `.` and `-`, starts with a risk word, never a mid-word substring like `display`);
    /// nothing binds. Held (the studio
    /// switch: `allow_with_conditions` over the obligations the starter carries), the same
    /// contexts bind exactly the documented obligations.
    #[test]
    fn wt_c4_the_testing_starter_reads_against_derived_plan_contexts() {
        let _ = wicked_apps_core::emit::hermetic_test_spool();
        let seed: Value = serde_json::from_str(include_str!(
            "../crates/wicked-governance/seed/testing/rules/testing-starter.json"
        ))
        .unwrap();
        let rules = wicked_governance::normalize_bundle(&seed, "filesystem").unwrap();
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        for r in &rules {
            wicked_governance::register_rule(&mut store, r).unwrap();
        }
        let eval = |store: &dyn wicked_apps_core::GraphRead, teamed: bool, paths: &[&str]| {
            let paths: Vec<String> = paths.iter().map(|p| p.to_string()).collect();
            let ctx = plan_context(&[], &paths, teamed, false, false, "20-39", false);
            StoreRules {
                store,
                projects: vec![],
                teamed,
            }
            .eval(&ctx)
            .unwrap()
        };
        let recalled = |store: &dyn wicked_apps_core::GraphRead, teamed: bool, paths: &[&str]| {
            let r = eval(store, teamed, paths);
            assert!(r.held.is_empty(), "the starter ships advisory: {r:?}");
            r.recalled
        };
        assert_eq!(
            recalled(&store, true, &["README.md", "docs/x.md"]),
            ["TST-1001"]
        );
        assert_eq!(recalled(&store, true, &["src/a.rs"]), ["TST-1002"]);
        assert_eq!(recalled(&store, true, &["a.json"]), ["TST-1002"]);
        assert_eq!(
            recalled(&store, true, &["README.md", "src/a.rs"]),
            ["TST-1002"]
        );
        assert!(recalled(&store, true, &["tests/t.rs"]).is_empty());
        assert_eq!(
            recalled(&store, true, &["src/checkout/pay.ts"]),
            ["TST-1002", "TST-1003"]
        );
        assert_eq!(
            recalled(&store, true, &["docs/payments/refunds.md"]),
            ["TST-1001", "TST-1003"]
        );
        assert_eq!(
            recalled(&store, true, &["src/auth_token.rs", "db/migrations/1.sql"]),
            ["TST-1002", "TST-1003"]
        );
        assert_eq!(
            recalled(&store, true, &["src/display.rs", "src/replay.ts"]),
            ["TST-1002"]
        );
        // Every documented risk word fires at the start of the path and after each separator,
        // and never inside a word (codex review: pin the regex both ways).
        for w in [
            "pay",
            "billing",
            "checkout",
            "invoice",
            "auth",
            "secur",
            "secret",
            "credential",
            "crypt",
            "migration",
            "schema",
            "deliver",
            "deploy",
            "release",
        ] {
            for path in [
                format!("{w}x.rs"),
                format!("src/{w}x.rs"),
                format!("src/a_{w}x.rs"),
                format!("src/a.{w}x"),
                format!("src/a-{w}x.rs"),
                format!("src/{}x.rs", w.to_uppercase()),
                format!("src/{w}/a.rs"),
                format!("src/a_{w}.rs"),
                format!("src/{w}"),
            ] {
                let r = recalled(&store, true, &[path.as_str()]);
                assert!(r.contains(&"TST-1003".to_string()), "{path}: {r:?}");
            }
            let inside = format!("src/x{w}.rs");
            let r = recalled(&store, true, &[inside.as_str()]);
            assert!(!r.contains(&"TST-1003".to_string()), "{inside}: {r:?}");
        }
        // Un-teamed, kinds fail closed to code: a docs touch still reads as a code change.
        assert_eq!(recalled(&store, false, &["README.md"]), ["TST-1002"]);

        // Held: the switch adds the effect; the obligations are the ones the starter carries.
        for r in &rules {
            if r.obligations.is_empty() {
                continue;
            }
            let mut held = r.clone();
            held.effect = Some(wicked_governance::Effect::AllowWithConditions);
            held.validate().unwrap();
            wicked_governance::register_rule(&mut store, &held).unwrap();
        }
        let ob = |rule: &str, token: &str| HeldObligation {
            rule: rule.into(),
            token: token.into(),
        };
        let r = eval(&store, true, &["src/checkout/pay.ts"]);
        assert_eq!(
            r.held,
            [
                ob("TST-1002", "step:test"),
                ob("TST-1002", "step:walkthrough"),
                ob("TST-1003", "step:security_review"),
            ]
        );
        assert!(r.recalled.is_empty(), "{:?}", r.recalled);
        let r = eval(&store, true, &["docs/guide.md"]);
        assert!(r.held.is_empty());
        assert_eq!(r.recalled, ["TST-1001"]);
    }
}
