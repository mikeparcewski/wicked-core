//! (studio#1170, crew#1512) THE GATE'S LEGAL ACTIONS: for a run awaiting a human, every answer
//! the engine would take at its open gate right now — the wire token, what it does AT THIS GATE,
//! whether it needs a note, and (for a reassign) the seats it would be accepted on.
//!
//! The list is not a second copy of the gate rules. Each candidate answer is run through the very
//! checks the decision path runs before it resolves a gate row — [`super::refuse_before_resolving`]
//! (after [`super::team_gate::refuse_answer`]), [`super::plan_answer_refusal`] for a plan gate, and
//! [`super::parked_reseat_refusal`] for a reassign's seats — and listed iff they accept it. A
//! skin renders exactly this list, so a card can no longer offer an answer the engine refuses, hide
//! one it takes, or label one as something it does not do.
//!
//! It writes nothing to the store and resolves no row. One inherited exception: the `rejudge` probe
//! on a `judge_unavailable` gate reads `registry_roster`, whose sign-in fill prepares seat config
//! directories (wicked-core#1522).

use serde::{Deserialize, Serialize};
use wicked_apps_core::GraphStore;

use crate::domain::{AgentSession, SessionStatus, WorkUnit};
use crate::workflow::{AmendScope, HumanDecision};

/// The text a probe answer carries where a note, reason or rationale is required: the checks only
/// ask whether one is there.
const PROBE: &str = "probe";

/// Whether an answer carries the operator's words (the gate body's `amend`, or a reassign's `note`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteRule {
    /// Send none: the engine refuses a note on this answer.
    None,
    /// The answer takes a note or goes without one.
    Optional,
    /// The answer is refused without one (a reason, a rationale, the amended acceptance).
    Required,
}

/// One answer the open gate takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateAction {
    /// The token the answer sends: `approve` | `reject` | `request_changes` | `edit_plan` | a
    /// decision `action` (`retry_qe`, `rejudge`, `retry_rules`, `skip_rules`, `skip_qe`,
    /// `operator_judgment`, `extend`, `targeted`, `accept_partial`, `accept_suggestion`,
    /// `amend_intent`, `consent:<id>`, a loop's choice) | `reassign` (`POST /runs/:id/reassign`).
    pub action: String,
    /// What it does AT THIS GATE — the label a skin draws comes from here, never from the prompt:
    /// `continue`, `retry`, `land_with_carried_items`, `operator_judgment`, `accept_work` (a team
    /// dispute's approve: the disputed work counts), `continue_team` (a team transport pause's
    /// approve: the missing team fact is published), `deliver`, `complete`,
    /// `approve_plan`, `send_back`, `stop`, `reseat_and_retry`, or the action token itself.
    pub intent: String,
    pub note: NoteRule,
    /// Set only when a note changes what the answer does: an approve with a note at a read-only
    /// phase's floor gate is a `floor_fix` (the note goes to a seat that can write). A required
    /// note on an adjudication's approve is `skip_rules`: Land records it as the skip reason for
    /// every open rule named in [`GateAction::rules`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note_intent: Option<String>,
    /// `reassign`: the seats the engine would re-seat the gated unit on now, roster order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub seats: Vec<String>,
    /// `reassign`: which of [`GateAction::seats`] this run has benched (taken on purpose — a seat
    /// signed back in — and said so), roster order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub benched_seats: Vec<String>,
    /// `skip_rules`: the rule ids a skip may name (each with its own reason). An adjudication's
    /// approve with `note_intent: "skip_rules"` skips all these open ids with the Land note.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<String>,
    /// A plan gate's answer names the held plan revision it answers (`planRev`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_plan_rev: bool,
}

/// The open gate and every answer it takes ([`crate::Core::gate_actions`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GateActions {
    /// The unit ord the gate paused before (the answer's `ord`).
    pub ord: u32,
    /// The open row's `gate_kind` (`def`, `escalation`, `review_adjudication`, `plan_approval`,
    /// `deliver`, …); `gate` when the row names none.
    pub gate_kind: String,
    /// A plan gate's held revision.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_rev: Option<u32>,
    /// In the order a skin offers them: approve, the named arms, send back, reassign, reject.
    /// EMPTY while nothing is answerable (a team fact still publishing).
    pub actions: Vec<GateAction>,
}

fn act(action: &str, intent: &str, note: NoteRule) -> GateAction {
    GateAction {
        action: action.to_string(),
        intent: intent.to_string(),
        note,
        note_intent: None,
        seats: Vec::new(),
        benched_seats: Vec::new(),
        rules: Vec::new(),
        needs_plan_rev: false,
    }
}

/// `Some(rule)` when an answer is taken with a note and/or without one.
fn note_rule(plain: bool, noted: bool) -> Option<NoteRule> {
    match (plain, noted) {
        (true, true) => Some(NoteRule::Optional),
        (true, false) => Some(NoteRule::None),
        (false, true) => Some(NoteRule::Required),
        (false, false) => None,
    }
}

/// Every answer run `run_id`'s open gate takes now; `None` when the run is not awaiting a human
/// (or its plan was sent back and is being re-planned: nothing is answerable until the next
/// revision is held).
pub(crate) fn gate_actions(
    store: &dyn GraphStore,
    run_id: &str,
) -> anyhow::Result<Option<GateActions>> {
    let session = crate::domain::get_session(store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    if session.status != SessionStatus::AwaitingHuman
        || session
            .team_plan
            .as_ref()
            .is_some_and(|t| t.send_back.is_some())
    {
        return Ok(None);
    }
    let open = crate::interaction::list_interactions(
        store,
        Some(run_id),
        Some(crate::interaction::InteractionStatus::Open),
    )?;
    let units = crate::domain::session_units(store, run_id)?;
    let cursor = units.get(session.unit_ix);
    let kind_open = |k: &str| open.iter().any(|r| r.gate_kind.as_deref() == Some(k));
    let ord = open
        .iter()
        .find_map(|r| r.ord)
        .or(cursor.map(|u| u.ord))
        .unwrap_or(0);

    // A plan gate answers only through its own arm ([`super::confirm_gate_expecting`]): its open
    // row, or a held plan whose row a crash left answered (re-opened there) — unless a team fact
    // is still pending, which the team handler answers.
    let team_fact_pending = session.team.as_ref().is_some_and(|t| t.pending.is_some());
    let held = session.team_plan.as_ref().and_then(|t| t.pending.as_ref());
    if kind_open(crate::plan_gate::GATE_KIND) || (held.is_some() && !team_fact_pending) {
        let plan_ord = open
            .iter()
            .find(|r| r.gate_kind.as_deref() == Some(crate::plan_gate::GATE_KIND))
            .and_then(|r| r.ord)
            .unwrap_or(ord);
        return Ok(Some(plan_actions(cursor, plan_ord, held.map(|p| p.rev))));
    }

    let gate_kind = open
        .iter()
        .find_map(|r| r.gate_kind.clone())
        .unwrap_or_else(|| "gate".to_string());
    let actions = if kind_open(super::CONSENT_GATE_KIND) {
        consent_actions(store, &session, &units, ord, &gate_kind)?
    } else if kind_open(crate::repeat::GATE_KIND) {
        repeat_actions(store, run_id, ord)?
    } else {
        answer_actions(store, &session, &units, &gate_kind)?
    };
    Ok(Some(GateActions {
        ord,
        gate_kind,
        plan_rev: None,
        actions,
    }))
}

/// A plan gate's answers, as [`super::plan_answer_refusal`] takes them.
fn plan_actions(cursor: Option<&WorkUnit>, ord: u32, rev: Option<u32>) -> GateActions {
    let not_run = cursor.is_some_and(|u| {
        matches!(
            u.status,
            crate::domain::UnitStatus::Pending | crate::domain::UnitStatus::Distributed
        )
    });
    let takes = |d: &HumanDecision| super::plan_answer_refusal(d, ord, not_run, true).is_ok();
    let approve = HumanDecision::Approve {
        amend: None,
        amend_scope: AmendScope::Cursor,
    };
    let edit = HumanDecision::EditPlan {
        plan: Default::default(),
        note: None,
    };
    let send_back = HumanDecision::RequestChanges { note: None };
    let mut actions = Vec::new();
    for (d, token, intent) in [
        (&approve, "approve", "approve_plan"),
        (&edit, "edit_plan", "edit_plan"),
        (&send_back, "request_changes", "send_back"),
        (&HumanDecision::Reject, "reject", "stop"),
    ] {
        if takes(d) {
            // A reject takes no note anywhere (the engine's `Reject` carries none).
            let note = if token == "reject" {
                NoteRule::None
            } else {
                NoteRule::Optional
            };
            // Every plan answer names the held revision, reject included: a stale card's reject
            // must never cancel a newer plan held at the same ord.
            let mut a = act(token, intent, note);
            a.needs_plan_rev = true;
            actions.push(a);
        }
    }
    GateActions {
        ord,
        gate_kind: crate::plan_gate::GATE_KIND.to_string(),
        plan_rev: rev,
        actions,
    }
}

/// A consent gate's answers (core#820): every ordinary answer the decision path's checks take
/// (approve = the plan's default; send back, an intent amendment, …), each of the dry-run plan's
/// choices after the approve, and reject. An Apps publish's consent takes only reject until it can
/// produce (the publish QE backstop), judged on the current worktree's tree just as the decision
/// path judges it. The snapshot confines new git objects to temporary storage (wicked-core#1534).
fn consent_actions(
    store: &dyn GraphStore,
    session: &AgentSession,
    units: &[WorkUnit],
    ord: u32,
    gate_kind: &str,
) -> anyhow::Result<Vec<GateAction>> {
    let reject = act("reject", "stop", NoteRule::None);
    if crate::apps::is_publish(session) {
        let current = super::current_worktree_tree_readonly(store, session);
        if crate::apps::publish_consent_refusal(session, units, current.as_deref()).is_some() {
            return Ok(vec![reject]);
        }
    }
    let offer = super::consent_offer(store, &session.id, ord)?;
    let mut actions = answer_actions(store, session, units, gate_kind)?;
    let at = actions
        .iter()
        .position(|a| a.action == "approve")
        .map_or(0, |i| i + 1);
    let choices: Vec<GateAction> = offer
        .plan
        .iter()
        .flat_map(|p| p.choices.iter())
        .map(|c| {
            let token = format!("{}{}", crate::consent_plan::CHOICE_PREFIX, c.id);
            act(&token, "consent_choice", NoteRule::None)
        })
        .collect();
    actions.splice(at..at, choices);
    Ok(actions)
}

/// A loop's ask (domain-graph-3), as `repeat_gate::resolve_answer` takes it: approve (the
/// command's recommended choice), one answer per choice, reject.
fn repeat_actions(
    store: &dyn GraphStore,
    run_id: &str,
    ord: u32,
) -> anyhow::Result<Vec<GateAction>> {
    let signal = super::repeat_gate::signal_for(store, run_id, ord)?;
    let mut actions: Vec<GateAction> = signal
        .iter()
        .flat_map(|s| s.choices.iter())
        .map(|c| {
            act(
                &crate::repeat::token(&c.id),
                "consent_choice",
                NoteRule::None,
            )
        })
        .collect();
    // A plain approve takes the command's recommended choice (else none); it carries no note.
    actions.insert(0, act("approve", "continue", NoteRule::None));
    actions.push(act("reject", "stop", NoteRule::None));
    Ok(actions)
}

/// Every other gate: each candidate answer run through the decision path's own pre-checks.
fn answer_actions(
    store: &dyn GraphStore,
    session: &AgentSession,
    units: &[WorkUnit],
    gate_kind: &str,
) -> anyhow::Result<Vec<GateAction>> {
    let run_id = session.id.as_str();
    let adjudication_open = gate_kind == crate::review_context::REVIEW_ADJUDICATION_GATE;
    let takes = |d: &HumanDecision| -> bool {
        super::team_gate::refuse_answer(session, d).is_ok()
            && super::floor_fix_route_for(store, session, run_id, d, adjudication_open)
                .and_then(|floor_fix| {
                    super::refuse_before_resolving(
                        store,
                        session,
                        run_id,
                        d,
                        None,
                        adjudication_open,
                        floor_fix,
                    )
                })
                .is_ok()
    };
    let cursor = units.get(session.unit_ix);
    let mut actions = Vec::new();

    // APPROVE: what it does here, and whether a note changes that.
    let plain = HumanDecision::Approve {
        amend: None,
        amend_scope: AmendScope::Cursor,
    };
    let noted = HumanDecision::Approve {
        amend: Some(PROBE.to_string()),
        amend_scope: AmendScope::Cursor,
    };
    let approve_intent = approve_intent(
        session,
        cursor,
        gate_kind,
        adjudication_open,
        super::team_gate::held_revision_applies(session, units),
    );
    if let Some(note) = note_rule(takes(&plain), takes(&noted)) {
        let mut a = act("approve", approve_intent, note);
        if note != NoteRule::None
            && super::floor_fix_route_for(store, session, run_id, &noted, adjudication_open)?
        {
            a.note_intent = Some("floor_fix".to_string());
        } else if note != NoteRule::None
            && super::team_gate::dispute_gate_open(session)
            && !super::team_gate::transport_gate_open(session)
        {
            // A team dispute's approve WITH a note is its send-back (`answer_dispute_gate`:
            // `DisputeAmended` reworks the step), never a continue.
            a.note_intent = Some("send_back".to_string());
        } else if adjudication_open && note == NoteRule::Required {
            if let Some(ids) = cursor
                .and_then(super::open_rule_ids)
                .filter(|ids| !ids.is_empty())
            {
                a.note_intent = Some("skip_rules".to_string());
                a.rules = ids;
            }
        }
        actions.push(a);
    }

    // The named arms, each by its own token.
    use crate::repo_checks::FloorRerunMode as Mode;
    let named: [(HumanDecision, &str, NoteRule); 9] = [
        (
            HumanDecision::RetryQe,
            crate::workflow::RETRY_QE_ACTION,
            NoteRule::None,
        ),
        (
            HumanDecision::Rejudge,
            crate::workflow::REJUDGE_ACTION,
            NoteRule::None,
        ),
        (
            HumanDecision::RecheckRules { skips: Vec::new() },
            crate::rule_check::RETRY_ACTION,
            NoteRule::None,
        ),
        (
            HumanDecision::OperatorJudgment {
                rationale: PROBE.to_string(),
            },
            crate::workflow::OPERATOR_JUDGMENT_ACTION,
            NoteRule::Required,
        ),
        (
            HumanDecision::SkipQe {
                reason: PROBE.to_string(),
            },
            crate::workflow::SKIP_QE_ACTION,
            NoteRule::Required,
        ),
        (
            HumanDecision::FloorRerun(Mode::Extend),
            "extend",
            NoteRule::None,
        ),
        (
            HumanDecision::FloorRerun(Mode::Targeted),
            "targeted",
            NoteRule::None,
        ),
        (
            HumanDecision::FloorRerun(Mode::AcceptPartial),
            "accept_partial",
            NoteRule::None,
        ),
        (
            HumanDecision::AcceptSuggestion,
            "accept_suggestion",
            NoteRule::None,
        ),
    ];
    for (d, token, note) in named.iter() {
        if takes(d) {
            actions.push(act(token, token, *note));
        }
    }
    // SKIP RULES: one probe per rule row of the gated unit; the rules a skip may name.
    let skippable: Vec<String> = cursor
        .and_then(|u| u.rule_check.as_ref())
        .map(|r| {
            r.rows
                .iter()
                .map(|row| row.rule_id.clone())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .filter(|id| {
            takes(&HumanDecision::RecheckRules {
                skips: vec![(id.clone(), PROBE.to_string())],
            })
        })
        .collect();
    if !skippable.is_empty() {
        let mut a = act(
            crate::rule_check::SKIP_ACTION,
            crate::rule_check::SKIP_ACTION,
            NoteRule::Required,
        );
        let mut seen = std::collections::HashSet::new();
        a.rules = skippable
            .into_iter()
            .filter(|r| seen.insert(r.clone()))
            .collect();
        actions.push(a);
    }
    if takes(&HumanDecision::AmendIntent {
        text: PROBE.to_string(),
    }) {
        actions.push(act("amend_intent", "amend_intent", NoteRule::Required));
    }

    // SEND BACK.
    let back_plain = HumanDecision::RequestChanges { note: None };
    let back_noted = HumanDecision::RequestChanges {
        note: Some(PROBE.to_string()),
    };
    if let Some(note) = note_rule(takes(&back_plain), takes(&back_noted)) {
        actions.push(act("request_changes", "send_back", note));
    }

    // REASSIGN: re-seat the gated unit, then approve it (crew's `POST /runs/:id/reassign`) — so
    // only where the plain approve RE-RUNS or RUNS the unit (never where it lands, judges or
    // delivers the work), on a seat step, on the seats the re-seat takes.
    // (studio#1170 codex-check) Only where that approve RE-RUNS or RUNS the step itself: never at a team pause, whose
    // approve answers the team (a dispute's counts the disputed work).
    if matches!(approve_intent, "retry" | "continue") && takes(&plain) {
        if let Some(unit) = cursor.filter(|u| u.tool_cmd.is_none()) {
            let current = unit.assigned_cli.as_deref().unwrap_or("claude");
            // Every roster seat the explicit re-seat takes — a benched one too (an operator may send
            // a step to a seat signed back in): `benched_seats` says which, so a skin can say so.
            let seats: Vec<String> = session
                .clis
                .iter()
                .filter(|k| k.as_str() != current)
                .filter(|k| super::refuse_ballot_only_target(k).is_ok())
                .filter(|k| super::parked_reseat_refusal(session, units, unit.ord, k).is_ok())
                .cloned()
                .collect();
            if !seats.is_empty() {
                // The reassign's note rides the approve that follows the re-seat: offered only where a
                // noted approve still re-runs the step (never where a note makes it a floor fix, which
                // would reuse the old output and never run the new seat — codex r3 on #1512).
                let noted_reruns = actions.iter().any(|a| {
                    a.action == "approve" && a.note != NoteRule::None && a.note_intent.is_none()
                });
                let note = if noted_reruns {
                    NoteRule::Optional
                } else {
                    NoteRule::None
                };
                let mut a = act("reassign", "reseat_and_retry", note);
                a.benched_seats = seats
                    .iter()
                    .filter(|k| session.benched_seats.iter().any(|b| &b.cli == *k))
                    .cloned()
                    .collect();
                a.seats = seats;
                actions.push(a);
            }
        }
    }

    if takes(&HumanDecision::Reject) {
        actions.push(act("reject", "stop", NoteRule::None));
    }
    Ok(actions)
}

/// What a plain approve does at this gate, read from the same facts the decision path branches on
/// ([`super::confirm_gate_expecting`]): the capped review's adjudication lands; a unit no judge or
/// QE reviewer could rule on is the operator's judgment (not on a team member's step); a deliver
/// or terminal gate delivers or completes; a denied unit is re-run; anything else runs on.
fn approve_intent(
    session: &AgentSession,
    cursor: Option<&WorkUnit>,
    gate_kind: &str,
    adjudication_open: bool,
    held_revision: bool,
) -> &'static str {
    if adjudication_open {
        return "land_with_carried_items";
    }
    // A team dispute's approve COUNTS the disputed work (`answer_dispute_gate`: `DisputeApproved`); a team transport
    // pause's approve publishes the missing team fact and the run goes on. Neither re-runs the step.
    // The transport pause first, as the decision path answers it: a transport pause over a dispute
    // runs the dispute's queued continuation, whatever it was (codex r3 on #1512).
    if super::team_gate::transport_gate_open(session) {
        return "continue_team";
    }
    if super::team_gate::dispute_gate_open(session) {
        return "accept_work";
    }
    if cursor
        .is_some_and(|u| u.member_step.is_none() && super::approve_is_operator_judgment(session, u))
    {
        return "operator_judgment";
    }
    match gate_kind {
        "deliver" => "deliver",
        // A terminal approve completes the run, unless a held revision applies first and runs the
        // step it added (`confirm_gate_expecting` → `apply_held_revision`).
        "terminal" if held_revision => "continue",
        "terminal" => "complete",
        // A failed step's gate (triage could not apply its flag, or escalated): approve re-runs it.
        "failure" | "triage" => "retry",
        _ if cursor.is_some_and(|u| u.status == crate::domain::UnitStatus::Rejected) => "retry",
        _ => "continue",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_apps_core::ToNode;

    /// A publish's choices must describe the tree the engine will judge when answered.
    #[test]
    fn publish_consent_offers_only_reject_after_the_reviewed_tree_changes() {
        let wt = crate::worktree_guard::tests::creator_worktree("publish-consent-tree");
        let repo = wt.parent().unwrap().join("repo");
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        let registered = crate::repo::register_repo(
            &mut store,
            crate::repo::RepoSpec {
                name: "publish-consent-tree".into(),
                root_path: repo.to_string_lossy().into_owned(),
                registered_at: 0,
            },
        )
        .unwrap();
        let run_id = "publish-consent-tree";
        let mut session = crate::actor::sessions_detail_fold_tests::session(run_id);
        let declared =
            crate::catalog::builtin_preset_instruments(crate::apps::APPS_PUBLISH).unwrap();
        session.assurance = crate::apps::contract(
            crate::assurance::RunAssurance::new(Some(&declared), false),
            Some(crate::apps::AppsContract {
                preset: crate::apps::APPS_PUBLISH.into(),
            }),
            &crate::assurance::QeOverride::Auto,
        )
        .unwrap();
        session.workdir = Some(wt.to_string_lossy().into_owned());
        session.repo_ref = Some(registered.id);
        session.unit_ix = 2;
        crate::domain::put_node(&mut store, session.to_node()).unwrap();
        let mut plan = WorkUnit::pending(format!("{run_id}:plan"), run_id, 1, "plan");
        plan.status = crate::domain::UnitStatus::Done;
        plan.tool_cmd = Some(vec!["unused".into()]);
        crate::domain::put_node(&mut store, plan.to_node()).unwrap();
        let outcome = crate::execute::apply_unit(
            &mut store,
            &plan,
            r#"{"dry_run":true,"choices":[{"id":"site","label":"Publish site","default":true}]}"#,
            &session.workflow_id,
            crate::scope::EntityMode::Shared,
            run_id,
            None,
            0,
        )
        .unwrap();
        assert!(outcome.approved);
        let mut qe = WorkUnit::pending(format!("{run_id}:qe"), run_id, 2, "qe");
        qe.repo_checks_floor = true;
        qe.role = crate::workflow::PhaseRole::Evaluator;
        qe.status = crate::domain::UnitStatus::Done;
        qe.assigned_cli = Some("claude".into());
        qe.qe_acceptance = session.assurance.qe.clone();
        let tree = crate::worktree_guard::snapshot(&wt, &repo).unwrap().tree;
        qe.qe_review = Some(crate::domain::QeReview {
            outcome: crate::qe_acceptance::review::OUTCOME_PASS.into(),
            reason: "PASS".into(),
            evidence_dir: Some("/ev/qe-1".into()),
            qe_run_id: Some("qe-1".into()),
            tree: Some(tree.clone()),
            verdict: Some("PASS".into()),
            attempts: vec![crate::domain::QeReviewAttempt {
                seat: "codex".into(),
                basis: crate::qe_acceptance::review::BASIS_DISTINCT.into(),
                same_cli: false,
                status: "verdict".into(),
                session: "nonce".into(),
                governed: true,
                tool_activity: false,
                attempt: 0,
            }],
        });
        let mut produce = WorkUnit::pending(format!("{run_id}:produce"), run_id, 3, "produce");
        produce.gate = crate::workflow::GateSpec::ConsentBefore;
        produce.tool_cmd = Some(vec!["unused".into()]);
        produce.depends_on = vec!["plan".into()];
        for unit in [&qe, &produce] {
            crate::domain::put_node(&mut store, unit.to_node()).unwrap();
        }
        let units = crate::domain::session_units(&store, run_id).unwrap();
        let offered = || {
            consent_actions(&store, &session, &units, 3, super::super::CONSENT_GATE_KIND)
                .unwrap()
                .into_iter()
                .map(|a| a.action)
                .collect::<Vec<_>>()
        };
        let unchanged = offered();
        assert!(unchanged.iter().any(|a| a == "approve"), "{unchanged:?}");
        assert!(
            unchanged.iter().any(|a| a == "consent:site"),
            "{unchanged:?}"
        );
        assert!(unchanged.iter().any(|a| a == "reject"), "{unchanged:?}");

        std::fs::write(wt.join("src/a.ts"), "changed after QE PASS\n").unwrap();
        assert_ne!(
            crate::worktree_guard::snapshot(&wt, &repo).unwrap().tree,
            tree
        );
        assert_eq!(offered(), ["reject"]);
        // An unreadable current tree must also fail closed with the same QE PASS present.
        std::fs::remove_dir_all(&wt).unwrap();
        assert_eq!(offered(), ["reject"]);
        std::fs::remove_dir_all(repo.parent().unwrap()).unwrap();
    }

    /// A failed step's gate (triage could not apply its flag, or escalated) re-runs the step.
    #[test]
    fn a_failure_or_triage_gates_approve_is_a_retry() {
        let session = crate::actor::sessions_detail_fold_tests::session("r");
        for kind in ["failure", "triage"] {
            assert_eq!(approve_intent(&session, None, kind, false, false), "retry");
        }
        assert_eq!(
            approve_intent(&session, None, "def", false, false),
            "continue"
        );
        assert_eq!(
            approve_intent(&session, None, "deliver", false, false),
            "deliver"
        );
        assert_eq!(
            approve_intent(&session, None, "terminal", false, false),
            "complete"
        );
        assert_eq!(
            approve_intent(&session, None, "terminal", false, true),
            "continue"
        );
        assert_eq!(
            approve_intent(&session, None, "x", true, false),
            "land_with_carried_items"
        );
    }
}
