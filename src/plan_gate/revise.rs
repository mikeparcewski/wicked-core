//! Seam T4 (DES-TEAMING-002 §8.7): re-deciding mid-run. The plan only GROWS: a revision adds
//! steps after the cursor, never removes one and never touches a unit already dispatched or done.
//!
//! Three triggers reach the actor, each on the actor's own command channel (it never reads the
//! bus):
//! 1. the PA's `PLAN+` line in its step output (`plan.proposed{kind:"change"}` →
//!    `plan.revised{reason:"pa_added"}`);
//! 2. a member's `change.requested`, only once the PA answers `PLAN <change_id>: ACCEPT` and
//!    restates the steps as its `PLAN+` line (`plan.revised{reason:"member_request"}`);
//! 3. the supervisor's diff re-score (`Command::TeamRescored`) whose band is above the run's
//!    ratcheted floor (`path.scored{basis:"diff"}` → `plan.revised{reason:"floor_raised"}`).
//!
//! This module is pure, like the rest of `plan_gate`: it builds the next plan state, the facts,
//! and the per-run def in RUNNING order (the done prefix first, then the logical plan's other
//! steps), and the actor applies them at the step boundary.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    approval, build, deliver_cmd, envelope, is_auto, per_run_def_id, plan_proposed, plan_refused,
    refusal_text, wire_step, AcceptedPlan, PendingPlan, PlanEvent, QueuedFact, Scored,
    TeamPlanState,
};
use crate::domain::HumanConfirm;
use crate::plan::{AddedBy, PlanStep, PlanSteps};
use crate::team::events::{self as ev, ProposalKind, ProposalSource, ReviseReason, TeamEvent};
use crate::workflow::WorkflowDef;

/// At most this many `PLAN+` lines are taken from one step's output.
pub(crate) const PLAN_BLOCK_MAX: usize = 8;

/// A diff re-score that raised the floor, waiting for the next step boundary (§8.7: a revision
/// applies there, never mid-unit). Only the highest one waiting is kept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffRescore {
    pub ord: u32,
    pub attempt: u32,
    pub rescore_seq: u32,
    pub score: u8,
    pub destructive: bool,
    /// Its `path.scored{basis:"diff"}`, published first at the boundary.
    pub fact: QueuedFact,
}

/// The settled diff's score (§8.7): the changed paths read as S4 signals against the run's graph,
/// the same graph and the same fail-closed rule as the intent score.
pub(crate) fn diff_score_for_run(
    paths: &[String],
    repo_root: Option<&std::path::Path>,
    base_commit: Option<&str>,
) -> Scored {
    let plan = PlanSteps {
        steps: Vec::new(),
        touch: Some(paths.to_vec()),
        floor_override: None,
    };
    super::intent_score_for_run(&plan, repo_root, base_commit)
}

/// Whether a score lands the run in a higher floor than its ratcheted one (§8.5: the band only
/// goes up), or newly high risk. A lower or equal score changes nothing.
pub(crate) fn floor_rises(state: &TeamPlanState, score: u8, destructive: bool) -> bool {
    let now = crate::review_scale::floor_for(state.max_score, state.destructive);
    let next = crate::review_scale::floor_for(
        state.max_score.max(score),
        state.destructive || destructive,
    );
    next.band != now.band || (next.high_risk && !now.high_risk)
}

/// `path.scored{basis:"diff"}` for a re-score (§6 row 2): keyed by `ord:attempt:rescore_seq`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn path_scored_diff(
    run_id: &str,
    ord: u32,
    attempt: u32,
    rescore_seq: u32,
    tree: Option<&str>,
    a: &crate::review_scale::Assessment,
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
        envelope(run_id, "engine", Some(ord), Some(attempt), now),
        json!({
            "score_source": ev::score_source_diff(ord, attempt, rescore_seq),
            "basis": "diff",
            "deterministic": a.deterministic,
            "reasons": a.reasons,
            "model": model,
            "signals": signals,
            "tree": tree,
        }),
    )
}

/// The PA's `PLAN` lines of one finished turn, held for the next advance (§8.7 triggers 1–2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanLines {
    /// The unit and attempt whose output carried them (the `PLAN+` proposal source).
    pub ord: u32,
    pub attempt: u32,
    /// The PA seat.
    pub by: String,
    /// Only the `PLAN` lines of the output.
    pub text: String,
}

/// The `PLAN` lines of a step output (the rest of the output is not kept).
pub(crate) fn plan_lines_of(output: &str) -> String {
    output
        .lines()
        .filter(|l| l.trim_start().starts_with("PLAN"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// What changes the plan.
pub(crate) enum Change {
    /// §8.7 trigger 3: a diff re-score raised the floor.
    Floor(DiffRescore),
    /// §8.7 triggers 1–2 (the PA's `PLAN+`, an accepted member request) and a human edit — at a
    /// mid-run plan gate, or mid-run through `Core::propose_plan`: steps to ADD.
    Steps {
        by: String,
        source: ProposalSource,
        kind: ProposalKind,
        /// The `plan.revised.reason`; `None` for a human edit (no `plan.revised`: its
        /// `plan.proposed{kind:"edit"}` and the rev's `plan.accepted` are the record).
        reason: Option<ReviseReason>,
        /// The human who made it approved it: an edit AT the gate (§8.6 "approve with amend"),
        /// accepted as the next rev `by:"human"` with no approval matrix. `false` for everything
        /// else — a `Core::propose_plan` edit included — so the matrix decides.
        approved_by_human: bool,
        steps: Vec<PlanStep>,
    },
}

/// What [`revise`] did.
pub(crate) enum Outcome {
    /// Accepted as `state.rev` (no approval needed, or the human made it); `def` runs.
    Accepted { def: WorkflowDef },
    /// Held for a `plan_approval` gate as `state.rev`; the units of `def` are inserted, and no
    /// unit dispatches until the gate is answered (T3's dispatch guard).
    Held { def: WorkflowDef },
    /// Refused (`plan.refused`): the run keeps its plan.
    Refused { reason: String },
}

pub(crate) struct Revised {
    pub state: TeamPlanState,
    pub events: Vec<TeamEvent>,
    pub outcome: Outcome,
}

fn catalog_pos(catalog: &str) -> Option<usize> {
    crate::catalog::catalog()
        .iter()
        .position(|e| e.id == catalog)
}

/// Insert `step` into `steps` at its catalog-order position (§8.7: new steps go in catalog order,
/// before `deliver`): after the last step whose catalog position is not after its own.
fn insert_by_catalog(steps: &mut Vec<PlanStep>, step: PlanStep) {
    let pos = catalog_pos(&step.catalog);
    let end = steps
        .iter()
        .position(|s| s.catalog == "deliver")
        .unwrap_or(steps.len());
    let at = steps[..end]
        .iter()
        .rposition(|s| catalog_pos(&s.catalog) <= pos)
        .map_or(0, |i| i + 1);
    steps.insert(at, step);
}

/// The running order (§8.7): the done prefix exactly as it ran, then every other step of the
/// logical plan in its order. A late step (its catalog position precedes a done step) therefore
/// lands at the cursor. `Err` when a done step is missing from the plan (a plan never shrinks).
pub(crate) fn running_order(logical: &PlanSteps, done: &[String]) -> Result<PlanSteps, String> {
    let mut steps = Vec::with_capacity(logical.steps.len());
    for d in done {
        let s = logical
            .steps
            .iter()
            .find(|s| &s.id == d)
            .ok_or_else(|| format!("the plan drops the step `{d}` that already ran"))?;
        steps.push(s.clone());
    }
    steps.extend(
        logical
            .steps
            .iter()
            .filter(|s| !done.contains(&s.id))
            .cloned(),
    );
    Ok(PlanSteps {
        steps,
        touch: logical.touch.clone(),
        floor_override: logical.floor_override.clone(),
    })
}

/// Revise the run's plan (§8.7): merge the change's steps into the working plan (a plan already
/// held at this boundary, else the accepted one), floor-fill at the ratcheted score, compose it in
/// running order, and run the approval matrix (§8.6) as a revision. Pure: the caller persists the
/// state, publishes the events and inserts the units of the def. `done` is the phase ids of the
/// units already dispatched or done, in order; `reviewing_ord` the unit whose boundary this is.
pub(crate) fn revise(
    run_id: &str,
    prior: &TeamPlanState,
    change: Change,
    done: &[String],
    human_confirm: &HumanConfirm,
    reviewing_ord: Option<u32>,
    now: i64,
) -> anyhow::Result<Revised> {
    let auto = is_auto(human_confirm);
    let base_rev = (prior.accepted_rev > 0).then_some(prior.accepted_rev);
    let base = match (&prior.pending, &prior.accepted) {
        (Some(p), _) => p.steps.clone(),
        (None, Some(a)) => a.steps.clone(),
        (None, None) => anyhow::bail!("run {run_id} has no accepted plan to revise"),
    };
    let mut events = Vec::new();
    let (additions, proposal_id, reason, score, destructive, by_human) = match change {
        Change::Floor(r) => {
            events.push(TeamEvent::from_payload(
                &r.fact.event_type,
                &r.fact.payload,
            )?);
            (
                Vec::new(),
                None,
                Some(ReviseReason::FloorRaised),
                r.score,
                r.destructive,
                false,
            )
        }
        Change::Steps {
            by,
            source,
            kind,
            reason,
            approved_by_human,
            steps,
        } => {
            let pid = ev::mint_proposal_id(run_id, &by, &source);
            let proposed = PlanSteps {
                steps: steps.clone(),
                touch: None,
                floor_override: None,
            };
            events.push(plan_proposed(
                run_id, &by, &pid, base_rev, kind, None, &proposed, now,
            )?);
            (steps, Some(pid), reason, 0, false, approved_by_human)
        }
    };
    let refuse = |mut events: Vec<TeamEvent>, reason: String| -> anyhow::Result<Revised> {
        if let Some(pid) = &proposal_id {
            events.push(plan_refused(run_id, pid, base_rev, &reason, now)?);
        } else {
            // A floor raise has no proposal to refuse: the path.scored stands, and the refusal
            // is the engine's own error (logged by the caller).
            anyhow::bail!("the floor raise could not be composed: {reason}");
        }
        Ok(Revised {
            state: prior.clone(),
            events,
            outcome: Outcome::Refused { reason },
        })
    };
    // §8.5 ratchet: the floor band is the maximum any score of the run has reached.
    let max_score = prior.max_score.max(score);
    let destructive = prior.destructive || destructive;
    // Provenance is output-only (floor fill refuses a step that supplies it): the base plan's is
    // set aside and restored on the same steps afterwards.
    let mut provenance: HashMap<String, (Option<AddedBy>, Option<String>)> = HashMap::new();
    let mut steps: Vec<PlanStep> = base
        .steps
        .iter()
        .cloned()
        .map(|mut s| {
            provenance.insert(s.id.clone(), (s.added_by.take(), s.floor_reason.take()));
            s
        })
        .collect();
    for s in additions {
        // The plan only grows: a step already in the plan is already there — by its id, or, for
        // a step that names no id, by its catalog entry (a restated or retried proposal adds
        // nothing twice; a deliberate second `review` names its own id).
        let present = if s.id.is_empty() {
            steps.iter().any(|b| b.catalog == s.catalog)
        } else {
            provenance.contains_key(&s.id)
        };
        if present {
            continue;
        }
        insert_by_catalog(&mut steps, s);
    }
    let merged = super::with_default_ids(&PlanSteps {
        steps,
        touch: base.touch.clone(),
        floor_override: base.floor_override.clone(),
    });
    if proposal_id.is_some() && merged.steps.len() == base.steps.len() {
        return refuse(events, "the proposal adds no step".to_string());
    }
    let deliver = deliver_cmd(prior.deliver_step.as_ref());
    let mut filled = match crate::plan::floor_fill(
        crate::catalog::catalog(),
        &merged,
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
    for s in &mut filled.steps.steps {
        if let Some((by, why)) = provenance.get(&s.id) {
            s.added_by = *by;
            s.floor_reason = why.clone();
        }
    }
    // The done prefix's catalog positions: a new step before any of them is late (§8.7).
    let done_pos: Vec<Option<usize>> = done
        .iter()
        .filter_map(|d| base.steps.iter().find(|s| &s.id == d))
        .map(|s| catalog_pos(&s.catalog))
        .collect();
    let added: Vec<(PlanStep, bool)> = filled
        .steps
        .steps
        .iter()
        .filter(|s| !provenance.contains_key(&s.id))
        .map(|s| {
            let p = catalog_pos(&s.catalog);
            let late = done_pos.iter().any(|d| *d > p);
            (s.clone(), late)
        })
        .collect();
    let running = match running_order(&filled.steps, done) {
        Ok(r) => r,
        Err(why) => return refuse(events, why),
    };
    let rev = prior.rev + 1;
    let mut def = match crate::plan::compose(crate::catalog::catalog(), &running) {
        Ok(d) => d,
        Err(r) => return refuse(events, refusal_text(&r)),
    };
    def.id = per_run_def_id(run_id, rev);
    let needs = if by_human {
        None
    } else if let Some(p) = &prior.pending {
        // A plan already held at this boundary stays held: the gate covers every revision.
        Some(p.reason.clone())
    } else {
        match approval(
            auto,
            filled.high_risk,
            filled.floor_override.is_some(),
            PlanEvent::Revision {
                previous_high_risk: prior.accepted_high_risk,
                approved_high_risk: prior.approved_high_risk,
            },
        ) {
            Ok(n) => n.map(|r| r.as_str().to_string()),
            Err(why) => return refuse(events, why.to_string()),
        }
    };
    if let Some(reason) = reason {
        let from = crate::review_scale::floor_for(prior.max_score, prior.destructive);
        let added_wire: Vec<serde_json::Value> = added
            .iter()
            .map(|(s, late)| {
                let mut v = wire_step(s);
                v["late"] = json!(late);
                v
            })
            .collect();
        events.push(build(
            ev::PLAN_REVISED,
            envelope(run_id, "engine", None, None, now),
            json!({
                "plan_rev": rev,
                "proposal_id": proposal_id,
                "reason": reason,
                "from_band": from.band,
                "to_band": filled.band,
                "high_risk": filled.high_risk,
                "added": added_wire,
            }),
        )?);
    }
    let mut state = prior.clone();
    state.rev = rev;
    state.max_score = max_score;
    state.destructive = destructive;
    let pid = proposal_id.unwrap_or_default();
    match needs {
        None => {
            state.accepted = Some(AcceptedPlan {
                rev,
                by: if by_human { "human" } else { "engine" }.to_string(),
                band: filled.band.clone(),
                high_risk: filled.high_risk,
                auto,
                steps: filled.steps.clone(),
                floor_override: filled.floor_override.clone(),
                proposal_id: pid,
            });
            state.accepted_rev = rev;
            state.accepted_high_risk = filled.high_risk;
            state.approved_high_risk |= by_human && filled.high_risk;
            state.pending = None;
            Ok(Revised {
                state,
                events,
                outcome: Outcome::Accepted { def },
            })
        }
        Some(reason) => {
            let mut floor_added: Vec<String> = prior
                .pending
                .as_ref()
                .map(|p| p.floor_added.clone())
                .unwrap_or_default();
            floor_added.extend(
                added
                    .iter()
                    .filter(|(s, _)| s.added_by == Some(AddedBy::Floor))
                    .map(|(s, _)| s.catalog.clone()),
            );
            state.pending = Some(PendingPlan {
                rev,
                proposal_id: pid,
                reviewing_ord,
                steps: filled.steps,
                band: filled.band,
                high_risk: filled.high_risk,
                floor_override: filled.floor_override,
                reason,
                floor_added,
                gate_id: None,
                refusal: None,
            });
            Ok(Revised {
                state,
                events,
                outcome: Outcome::Held { def },
            })
        }
    }
}

/// One parsed `PLAN` line of a step's output (§8.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PlanLine {
    /// `PLAN <change_id>: ACCEPT|DECLINE — <reason>`: the PA's answer to a member's request.
    Answer { change_id: String, accept: bool },
    /// `PLAN+ {"steps":[…],"reason":"…"}`: steps to add, on one line.
    Block { steps: Vec<PlanStep> },
}

/// The PA's `PLAN` lines, in order. A `PLAN+` whose JSON does not parse into at least one step is
/// not a block; at most [`PLAN_BLOCK_MAX`] blocks are taken.
pub(crate) fn parse_plan_lines(output: &str) -> Vec<PlanLine> {
    #[derive(Deserialize)]
    struct Block {
        steps: Vec<PlanStep>,
        #[serde(default)]
        #[allow(dead_code)]
        reason: String,
    }
    let mut out = Vec::new();
    let mut blocks = 0;
    for line in output.lines() {
        let line = line.trim_start();
        if let Some(json) = line.strip_prefix("PLAN+") {
            if blocks >= PLAN_BLOCK_MAX {
                continue;
            }
            if let Ok(b) = serde_json::from_str::<Block>(json.trim()) {
                if !b.steps.is_empty() {
                    blocks += 1;
                    out.push(PlanLine::Block { steps: b.steps });
                }
            }
        } else if let Some(rest) = line.strip_prefix("PLAN ") {
            let Some((id, rest)) = rest.split_once(':') else {
                continue;
            };
            let id = id.trim();
            if id.is_empty() || id.contains(char::is_whitespace) {
                continue;
            }
            let rest = rest.trim_start();
            let accept = if rest.starts_with("ACCEPT") {
                true
            } else if rest.starts_with("DECLINE") {
                false
            } else {
                continue;
            };
            out.push(PlanLine::Answer {
                change_id: id.to_string(),
                accept,
            });
        }
    }
    out
}

/// The step output's plan changes, in order (§8.7 triggers 1–2): a `PLAN+` right after the PA's
/// `PLAN <change_id>: ACCEPT` restates that member request (`member_request`, sourced by its
/// `change_id`); any other `PLAN+` is the PA's own (`pa_added`, sourced `ord:attempt:seq`).
pub(crate) fn changes_from_output(output: &str, by: &str, ord: u32, attempt: u32) -> Vec<Change> {
    let mut out = Vec::new();
    let mut accepted: Option<String> = None;
    let mut seq = 0u32;
    for line in parse_plan_lines(output) {
        match line {
            PlanLine::Answer { change_id, accept } => {
                accepted = accept.then_some(change_id);
            }
            PlanLine::Block { steps } => {
                seq += 1;
                let (source, reason) = match accepted.take() {
                    Some(change_id) => (
                        ProposalSource::Change { change_id },
                        ReviseReason::MemberRequest,
                    ),
                    None => (
                        ProposalSource::PlanBlock {
                            ord,
                            attempt,
                            plan_block_seq: seq,
                        },
                        ReviseReason::PaAdded,
                    ),
                };
                out.push(Change::Steps {
                    by: by.to_string(),
                    source,
                    kind: ProposalKind::Change,
                    reason: Some(reason),
                    approved_by_human: false,
                    steps,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests;
