//! (DES-STANDIN-001 §3.8, §3.10) THE `stand_in_needed` GATE: a stand-in-aware run whose
//! independent check has no independent helper, and no consent to stand one in, parks here
//! instead of failing or checking itself. The gate takes three answers:
//!
//! - `use_stand_ins{scope}` ([`crate::Core::grant_stand_ins`]): the person's consent goes on the
//!   run's contract (`via: "grant"`), and exactly the held check continues on a stand-in;
//! - approve (retry): real helpers first (one may have been signed in since), a stand-in only if
//!   consent exists by then; still short, the gate opens again;
//! - reject: the run stops.
//!
//! Plan time is the one hold today: distribution returns [`StandInNeeded`] and the run parks on
//! its cursor before anything ran, exactly as an all-benched roster parks at `dead_seat`. The
//! answers re-route the plan's not-yet-run units over the run's roster and contract.

use std::sync::mpsc::Sender;

use wicked_apps_core::{GraphStore, ToNode};

use crate::command::Command;
use crate::distribute::StandInNeeded;
use crate::domain::{AgentSession, SessionStatus, WorkUnit};
use crate::event::CoreEvent;

/// The denial source (and gate class) of a check held for want of consent.
pub(crate) const DENIAL_SOURCE_STAND_IN_NEEDED: &str =
    crate::standin::DENIAL_SOURCE_STAND_IN_NEEDED;

/// Whether the run's open gate is a `stand_in_needed` hold on its cursor.
pub(crate) fn gate_open(cursor: Option<&WorkUnit>) -> bool {
    cursor
        .and_then(|u| u.denial.as_ref())
        .is_some_and(|d| d.source == DENIAL_SOURCE_STAND_IN_NEEDED)
}

/// (T-R11) Whether the hold on `unit` is its QE reviewer's: the gate is `stand_in_needed` and the
/// unit's isolated QE review found no seat to run on. Its answer re-dispatches the QE unit (the
/// dispatch seats the reviewer), never a judge-only re-run.
pub(crate) fn qe_reviewer_held(unit: &WorkUnit) -> bool {
    gate_open(Some(unit))
        && crate::qe_acceptance::review::applies(unit)
        && unit
            .qe_review
            .as_ref()
            .is_some_and(|r| r.outcome == crate::qe_acceptance::review::OUTCOME_UNAVAILABLE)
}

/// (§3.8, rev 3 N4) Refuse a `use_stand_ins` answer the run cannot take: not at this gate, on a
/// run that is not stand-in-aware, on a reduced run, or with a scope that is not `job` | `always`.
pub(crate) fn refuse_use_stand_ins(
    session: &AgentSession,
    cursor: Option<&WorkUnit>,
    scope: &str,
    actor: Option<&str>,
) -> anyhow::Result<()> {
    if !gate_open(cursor) {
        anyhow::bail!(
            "action `{}` answers only a stand_in_needed gate",
            crate::workflow::USE_STAND_INS_ACTION
        );
    }
    if !crate::standin::active(&session.assurance) || session.assurance.reduced() {
        anyhow::bail!(
            "run {} is not stand-in-aware (or runs reduced): it cannot take stand-ins",
            session.id
        );
    }
    if scope != crate::standin::SCOPE_JOB && scope != crate::standin::SCOPE_ALWAYS {
        anyhow::bail!(
            "action `{}` needs scope `job` or `always`, not `{scope}`",
            crate::workflow::USE_STAND_INS_ACTION
        );
    }
    // (core#1864) A grant is consent, and consent is a person's: a grant naming no one is refused
    // as a blank launch consent is (`StandInConsent::validate`), never recorded as `operator`.
    if actor.is_none_or(|a| a.trim().is_empty()) {
        anyhow::bail!(
            "action `{}` names no one (`actor`): consent to stand-ins is a person's",
            crate::workflow::USE_STAND_INS_ACTION
        );
    }
    Ok(())
}

/// The gate's words: which check is held, why, and the three answers.
pub(crate) fn prompt(unit: &WorkUnit, reason: &str, note: &str) -> String {
    // A unit whose seat already ran waits for its check; one that never ran waits to start.
    let held = if unit.last_attempt.is_some() {
        "its work is done and waits for its check"
    } else {
        "it is held before it runs"
    };
    format!(
        "Unit {}: {held}. {}. Use a stand-in (for this job, or for every job), sign another \
         helper in and approve to retry, or reject to stop the run{note}",
        unit.ord,
        super::reason_head(reason)
    )
}

/// (§3.2) Park the run at the `stand_in_needed` gate on its cursor, the plan's distribution having
/// come up short with no consent. Mirrors the dead-seat park: every not-yet-seated unit is seated
/// provisionally on the roster's first seat, the cursor carries the hold, and the held check is
/// persisted on the run's contract (`pendingStandIn`) for the answer to continue. `Ok(false)` when
/// the run is already terminal.
pub(crate) fn park(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    self_tx: &Sender<Command>,
    run_id: &str,
    need: &StandInNeeded,
) -> anyhow::Result<bool> {
    let Some(mut session) = crate::domain::get_session(store, run_id)? else {
        anyhow::bail!("run not found: {run_id}");
    };
    if matches!(
        session.status,
        SessionStatus::Cancelled | SessionStatus::Failed | SessionStatus::Completed
    ) {
        return Ok(false);
    }
    for seat in &need.benched_seats {
        crate::domain::bench_seat(&mut session.benched_seats, seat.clone());
    }
    let mut units = crate::domain::session_units(store, run_id)?;
    if units.is_empty() {
        anyhow::bail!("run {run_id} has no units to park");
    }
    let roster = crate::registry_roster();
    let seat = session.clis.first().cloned();
    let invocation = seat
        .as_deref()
        .and_then(|k| crate::distribute::invocation_of(&roster, k));
    for u in units
        .iter_mut()
        .filter(|u| u.tool_cmd.is_none() && u.assigned_cli.is_none())
    {
        u.assigned_cli = seat.clone();
        u.assigned_invocation = invocation.clone();
    }
    let ix = session.unit_ix.min(units.len() - 1);
    let why = wicked_apps_core::emit::redact_paths(&need.to_string());
    units[ix].denial_reason = Some(why.clone());
    units[ix].denial = Some(crate::domain::UnitDenial::new(
        DENIAL_SOURCE_STAND_IN_NEEDED,
        why,
    ));
    let attempt = session.attempt;
    session.assurance.pending_stand_in = Some(crate::assurance::PendingStandIn {
        role: need.role,
        ord: need.ords.first().copied().unwrap_or(units[ix].ord),
        attempt,
        subject: None,
        evidence: None,
        reason: need.reason.clone(),
    });
    let nodes: Vec<_> = units.iter().map(|u| u.to_node()).collect();
    crate::domain::put_nodes(store, &nodes)?;
    let note = super::denial_gate_note(session.human_confirm);
    super::escalate_denied_unit(
        store,
        subscribers,
        self_tx,
        &mut session,
        &units[ix],
        attempt,
        false,
        false,
        false,
        note,
    )?;
    Ok(true)
}

/// What an answer at the gate does next.
pub(crate) enum Answered {
    /// The held check can run now: answer as a plain approve (the cursor re-dispatches).
    Run,
    /// Still short with no consent: the gate opened again.
    Held,
}

/// (§3.10) Answer the gate: record the grant (a `use_stand_ins`), then re-route the plan's
/// not-yet-run units over the run's roster and contract. Real helpers are taken first; a stand-in
/// only with consent; still short, the gate opens again.
#[allow(clippy::too_many_arguments)]
pub(crate) fn answer(
    store: &mut dyn GraphStore,
    events: &mut crate::event_log::EventSink,
    self_tx: &Sender<Command>,
    session: &mut AgentSession,
    run_id: &str,
    grant: Option<&str>,
    actor: Option<&str>,
) -> anyhow::Result<Answered> {
    if let Some(scope) = grant {
        let consent = crate::standin::StandInConsent {
            scope: scope.to_string(),
            // Never blank: `refuse_use_stand_ins` refused a grant naming no one.
            by: actor.map(str::trim).unwrap_or_default().to_string(),
            at: crate::interaction::now_millis(),
            via: "grant".to_string(),
        };
        let ord = session
            .assurance
            .pending_stand_in
            .as_ref()
            .map_or(session.unit_ix as u32, |p| p.ord);
        session.assurance.stand_in = Some(consent.clone());
        super::emit(
            events,
            CoreEvent::StandInsGranted {
                session: run_id.to_string(),
                ord,
                consent,
            },
        );
    }
    // A hold whose seat already ran (a missing judge) re-routes nothing: only its judge runs again,
    // on the reviewed attempt (the caller arms the re-judge). Completed work is never re-seated.
    let cursor_ran = crate::domain::session_units(store, run_id)?
        .get(session.unit_ix)
        .is_some_and(|u| u.last_attempt.is_some());
    let routed = if cursor_ran {
        Ok(())
    } else {
        reroute(store, events, session)
    };
    match routed {
        Ok(()) => {
            session.assurance.pending_stand_in = None;
            crate::domain::put_node(store, session.to_node())?;
            Ok(Answered::Run)
        }
        Err(e) => match e.downcast_ref::<StandInNeeded>() {
            Some(need) => {
                crate::domain::put_node(store, session.to_node())?;
                if !park(store, events, self_tx, run_id, need)? {
                    anyhow::bail!("run {run_id} ended while its stand-in gate was answered");
                }
                // The park wrote the session; the caller's copy is stale.
                if let Some(s) = crate::domain::get_session(store, run_id)? {
                    *session = s;
                }
                Ok(Answered::Held)
            }
            None => Err(e),
        },
    }
}

/// Route the plan again over the run's roster (in its order) and contract, and re-seat every
/// unit that has not run: its seat, template and routing, and any stand-in it now has, persisted
/// on the contract and announced before anything dispatches.
fn reroute(
    store: &mut dyn GraphStore,
    events: &mut crate::event_log::EventSink,
    session: &mut AgentSession,
) -> anyhow::Result<()> {
    let run_id = session.id.clone();
    let mut units = crate::domain::session_units(store, &run_id)?;
    // (codex r1 B16) The launch's own records when the plan state kept them (a team run's
    // `team_plan.roster`: instances and launch overrides as admitted); otherwise the run's keys over
    // the registry, as the dispatch resolves them.
    let launch = super::launch_roster(session)?;
    let roster: Vec<crate::AgenticCli> = if launch.is_empty() {
        let registry = crate::registry_roster();
        session
            .clis
            .iter()
            .filter_map(|k| registry.iter().find(|c| &c.key == k).cloned())
            .collect()
    } else {
        launch
    };
    // The run's live benches stand: consent is no evidence that a credential recovered (a bench
    // lifts through its own re-probe).
    let dists = crate::distribute::distribute_units_on_benched(
        &units,
        &roster,
        &run_id,
        &session.benched_seats,
        &session.assurance,
    )?;
    let mut changed = Vec::new();
    for (u, d) in units.iter_mut().zip(dists) {
        if u.tool_cmd.is_some() || u.last_attempt.is_some() {
            continue;
        }
        if let Some(b) = &d.stand_in {
            if !session.assurance.stand_ins.iter().any(|x| x.key == b.key) {
                session.assurance.stand_ins.push(b.clone());
            }
            let display = crate::standin::display_name(&roster, &b.base);
            crate::standin::note_label(&mut session.assurance, b, &display);
            super::emit(
                events,
                CoreEvent::StandInSeated {
                    session: run_id.clone(),
                    ord: Some(u.ord),
                    binding: b.clone(),
                    label: crate::standin::label(b, &display),
                },
            );
        }
        u.assigned_cli = Some(d.assigned_cli);
        u.assigned_invocation = d.assigned_invocation;
        u.routing = Some(d.routing);
        u.pool_seating = d.pool;
        changed.push(u.to_node());
    }
    crate::domain::put_nodes(store, &changed)?;
    Ok(())
}
