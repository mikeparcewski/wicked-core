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

/// (DES-STANDIN-001 §3.2, core#1972, core#1973) `Command::ReserveStandIns`: reserve `need`
/// stand-ins for a team `role` (a council, the monitors) on the run's own seats, on the single
/// writer. Each is a fresh binding with its lens by position, persisted on the run's contract and
/// announced (`StandInSeated`) before the reply, so two reservations never mint one key. Empty
/// unless the run is stand-in-aware, consented and not reduced, and one of its seats can stand in:
/// not benched, not launch-excluded, with a record the role can run (a council voter is enabled for
/// the council). The caller asks only when no real seat is left.
pub(crate) fn reserve_team(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    role: crate::standin::Role,
    need: usize,
    scope: crate::standin::BindingScope,
) -> anyhow::Result<Vec<crate::standin::SeatedStandIn>> {
    use crate::standin as si;
    use wicked_apps_core::spawn::seat_id;
    let Some(mut session) = crate::domain::get_session(store, run_id)? else {
        return Ok(Vec::new());
    };
    let run = &session.assurance;
    if need == 0 || !si::active(run) || run.stand_in.is_none() || run.reduced() {
        return Ok(Vec::new());
    }
    if matches!(
        session.status,
        SessionStatus::Cancelled | SessionStatus::Failed | SessionStatus::Completed
    ) {
        return Ok(Vec::new());
    }
    let registry = crate::registry_roster();
    let launch = super::launch_roster(&session)?;
    let roster: Vec<crate::AgenticCli> = if launch.is_empty() {
        session
            .clis
            .iter()
            .filter_map(|k| registry.iter().find(|c| &c.key == k).cloned())
            .collect()
    } else {
        launch
    };
    let mut bases: Vec<String> = Vec::new();
    for k in &session.clis {
        if seat_id::is_stand_in(k) {
            continue;
        }
        let c = seat_id::credential(k).to_string();
        let benched = session
            .benched_seats
            .iter()
            .any(|b| seat_id::credential(&b.cli) == c);
        let excluded = session
            .exclude_seats
            .iter()
            .any(|e| e == &c || e.as_str() == seat_id::family(&c));
        let runnable =
            si::base_record(&roster, &registry, &si::binding_key(&c, 1)).is_some_and(|r| {
                !r.headless_invocation.trim().is_empty()
                    && (role != si::Role::Council || r.enabled_for_council)
            });
        if !benched && !excluded && runnable && !bases.contains(&c) {
            bases.push(c);
        }
    }
    let Some(first) = bases.first().cloned() else {
        return Ok(Vec::new());
    };
    let reason = si::reason_from_run(&roster, &session.clis, &session.benched_seats, &first);
    let minted = match si::fill(si::FillRequest {
        role,
        need,
        real: Vec::new(),
        bases,
        builder_model: None,
        consent: run.stand_in.as_ref(),
        reason,
        scope,
        taken: &run.stand_ins,
    }) {
        si::Fill::Seats { stand_ins, .. } => stand_ins,
        si::Fill::NeedConsent(_) | si::Fill::NoUsableBase(..) => return Ok(Vec::new()),
    };
    let mut seated = Vec::new();
    for (i, mut b) in minted.into_iter().enumerate() {
        let Some(mut record) = si::base_record(&roster, &registry, &b.key).cloned() else {
            continue;
        };
        record.key = b.key.clone();
        b.lens = Some(si::lens_at(i).to_string());
        let display = si::display_name(&roster, &b.base);
        b.display = Some(display.clone());
        let label = si::label(&b, &display);
        seated.push(si::SeatedStandIn {
            binding: b,
            record,
            label,
        });
    }
    for s in &seated {
        session.assurance.stand_ins.push(s.binding.clone());
        si::note_label(&mut session.assurance, &s.binding, s.binding.display());
    }
    if seated.is_empty() {
        return Ok(seated);
    }
    crate::domain::put_node(store, session.to_node())?;
    for s in &seated {
        super::emit(
            subscribers,
            CoreEvent::StandInSeated {
                session: run_id.to_string(),
                ord: s.binding.ord,
                label: s.label.clone(),
                binding: s.binding.clone(),
            },
        );
    }
    Ok(seated)
}

#[cfg(test)]
mod reserve_team_tests {
    //! (core#1972, core#1973) `Command::ReserveStandIns` on the single writer.
    use super::*;
    use crate::standin::{self as si, Role};

    fn run(clis: &[&str], consent: bool) -> AgentSession {
        let mut s = crate::actor::sessions_detail_fold_tests::session("r-team");
        s.clis = clis.iter().map(|c| c.to_string()).collect();
        s.status = SessionStatus::Executing;
        s.assurance = si::LaunchStandIn {
            aware: true,
            parties: Vec::new(),
            consent: consent.then(|| si::StandInConsent {
                scope: si::SCOPE_JOB.into(),
                by: "operator".into(),
                at: 1,
                via: "launch".into(),
            }),
        }
        .apply(s.assurance);
        s
    }

    fn reserve(
        s: &AgentSession,
        role: Role,
        need: usize,
    ) -> (Vec<si::SeatedStandIn>, AgentSession, Vec<CoreEvent>) {
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        crate::domain::put_node(&mut store, s.to_node()).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut sink = crate::event_log::EventSink::default();
        sink.push(tx);
        let scope = si::BindingScope {
            ord: Some(3),
            attempt: Some(1),
            ..Default::default()
        };
        let seated = reserve_team(&mut store, &mut sink, &s.id, role, need, scope).unwrap();
        let after = crate::domain::get_session(&store, &s.id).unwrap().unwrap();
        (seated, after, rx.try_iter().collect())
    }

    /// A council of three on a one-CLI consented run: three fresh bindings on the one seat, each
    /// with its own lens by position, on the base's record re-keyed to the binding; persisted on
    /// the run's contract and announced before the reply. A second reservation never reuses a slot.
    #[test]
    fn a_team_council_reserves_fresh_stand_ins_with_a_lens_each_1972() {
        let (seated, after, events) = reserve(&run(&["claude"], true), Role::Council, 3);
        let keys: Vec<&str> = seated.iter().map(|s| s.binding.key.as_str()).collect();
        assert_eq!(keys, ["claude~1", "claude~2", "claude~3"]);
        let lenses: Vec<Option<&str>> = seated.iter().map(|s| s.binding.lens.as_deref()).collect();
        assert_eq!(
            lenses,
            [
                Some("correctness"),
                Some("security and safety"),
                Some("user impact")
            ]
        );
        for s in &seated {
            assert_eq!(s.binding.role, Role::Council);
            assert_eq!(s.record.key, s.binding.key);
            assert_eq!((s.binding.ord, s.binding.attempt), (Some(3), Some(1)));
            assert!(s.label.contains("stood in for the council"), "{}", s.label);
            assert!(
                s.label.contains("only Claude Code was picked for this job"),
                "{}",
                s.label
            );
        }
        assert_eq!(
            after.assurance.stand_ins.len(),
            3,
            "persisted before the reply"
        );
        let announced = events
            .iter()
            .filter(|e| matches!(e, CoreEvent::StandInSeated { .. }))
            .count();
        assert_eq!(announced, 3);
        // The next reservation takes the next slots.
        let (next, _, _) = reserve(&after, Role::Monitor, 1);
        assert_eq!(next[0].binding.key, "claude~4");
        assert_eq!(next[0].binding.lens.as_deref(), Some("correctness"));
    }

    /// No consent, an unaware run, a reduced run, a benched base or no seat at all: nothing is
    /// reserved and nothing is written.
    #[test]
    fn no_team_stand_in_without_consent_awareness_or_a_usable_base_1972() {
        for (why, s) in [
            ("no consent", run(&["claude"], false)),
            ("unaware", {
                let mut s = run(&["claude"], true);
                s.assurance.stand_in_aware = false;
                s
            }),
            ("reduced", {
                let mut s = run(&["claude"], true);
                s.assurance.mode = crate::assurance::MODE_REDUCED.into();
                s
            }),
            ("benched", {
                let mut s = run(&["claude"], true);
                s.benched_seats.push(crate::domain::BenchedSeat {
                    cli: "claude".into(),
                    reason: "quota_exhausted".into(),
                    source: "worker".into(),
                    governed_only: false,
                    benched_until: None,
                    strikes: 1,
                    replaced: None,
                });
                s
            }),
            ("no seat", run(&[], true)),
        ] {
            let (seated, after, _) = reserve(&s, Role::Council, 3);
            assert!(seated.is_empty(), "{why}");
            assert!(after.assurance.stand_ins.is_empty(), "{why}");
        }
    }
}
