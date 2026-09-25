//! The actor's half of reliable team publishing (DES-TEAMING-002 §4.1, seam P1): the required
//! transitions, the `team_transport` pause and its three answers, the acknowledgement handlers,
//! the un-teamed unit snapshot, and the boot reconcile.
//!
//! **The actor never blocks on a publish.** It reaches the bus only by sending a
//! [`PublisherReq`] to the [`TeamLink`] publisher thread and DEFERRING the dependent step until
//! `Command::TeamPublished` / `TeamTransportFailed` / `TeamSuperseded` comes back
//! (`Progress::Deferred`). It never opens the bus and never takes the outbox lock at runtime.
//!
//! **Required transitions fail closed (§4.1 table).**
//! - `path.started` gates a team run's first dispatch. Past the bound the publisher tombstones the
//!   run's lines and only then reports the failure; the actor then persists `transport: none` and
//!   dispatches un-teamed, every unit stamped `transport: none`.
//! - `plan.accepted` gates the plan's first dispatch. Past the bound the run pauses
//!   `team_transport`, the reason in the prompt.
//! - `gate.decided` gates the resume after a team gate is answered. Past the bound the run stays
//!   paused `team_transport` (no `Resumed`, no dispatch).
//!
//! No CoreEvent is added: the fallback is disclosed by the persisted state (`AgentSession.team`,
//! each unit's snapshot) and the `team_transport` pause (§4.6).

use std::cell::RefCell;

use super::*;
use crate::domain::{
    PendingStage, PendingTeamFact, RunTeamState, TeamBlocked, UnitTeamSnapshot, WorkUnit,
};
use crate::team::events::{
    self as tev, Envelope, GateDecided, GateDecision, GateKind, GateOpened, GateOpenedKind,
    LedgerSource, PathEnded, PathStarted, PathStatus, PlanAccepted, PlanMode, PlanStep, Selection,
    TeamBody, TeamEvent, Transport,
};
use crate::team::publish::{Exhausted, PublisherReq, TeamLink, TeamToken};

thread_local! {
    /// The actor thread's link to the team publisher, installed once by [`install`].
    static TEAM_LINK: RefCell<Option<TeamLink>> = const { RefCell::new(None) };
}

/// Install this actor thread's team link (called once at the top of `actor::run`).
pub(super) fn install(link: TeamLink) {
    TEAM_LINK.with(|c| *c.borrow_mut() = Some(link));
}

fn link() -> Option<TeamLink> {
    TEAM_LINK.with(|c| c.borrow().clone())
}

fn send(req: PublisherReq) -> Result<(), String> {
    match link() {
        Some(TeamLink::Publisher { tx, .. }) => tx
            .send(req)
            .map_err(|_| "the team publisher stopped".to_string()),
        Some(TeamLink::Unavailable(reason)) => Err(reason),
        None => Err("no team publisher on this engine".to_string()),
    }
}

/// The plan rev P1 accepts: the launch's composed plan. Re-plans (`plan.revised`) are T4's.
pub(super) const PLAN_REV: u32 = 1;

/// The amend text that answers a `team_transport` pause with "continue without team" (§4.1).
pub const CONTINUE_WITHOUT_TEAM: &str = "continue without team";

/// The `gate_kind` of the pause a failed required fact opens.
pub const TEAM_TRANSPORT_GATE: &str = "team_transport";

/// Whether this run is a team run: its units were planned from the run's composed def (D1's
/// marker), or it already carries team state.
pub(super) fn is_team_run(session: &AgentSession, units: &[WorkUnit]) -> bool {
    session.team.is_some() || units.iter().any(|u| u.team_run)
}

fn envelope(run_id: &str) -> Envelope {
    Envelope {
        run_id: run_id.to_string(),
        ord: None,
        attempt: None,
        by: "engine".to_string(),
        at: crate::interaction::now_millis(),
        re: None,
    }
}

fn event(run_id: &str, body: TeamBody) -> TeamEvent {
    TeamEvent {
        env: envelope(run_id),
        body,
    }
}

fn path_started(session: &AgentSession) -> TeamEvent {
    event(
        &session.id,
        TeamBody::PathStarted(PathStarted {
            cli: session.clis.first().cloned().unwrap_or_default(),
            selection: Selection::Chosen,
            roster: session.clis.clone(),
            request: crate::team::cap_utf8(&session.problem, 8 * 1024),
            workflow: None,
            plan: false,
        }),
    )
}

/// `plan.accepted` for the launch's composed plan. The steps name each unit's phase; the scored
/// band and high-risk flag are T2's and ride empty/false until then.
fn plan_accepted(session: &AgentSession, units: &[WorkUnit], plan_rev: u32) -> TeamEvent {
    event(
        &session.id,
        TeamBody::PlanAccepted(PlanAccepted {
            plan_rev,
            workflow_id: format!("{}:plan-{plan_rev}", session.id),
            band: String::new(),
            high_risk: false,
            mode: PlanMode::Auto,
            steps: units
                .iter()
                .filter(|u| u.team_run)
                .map(|u| {
                    let id = u
                        .phase_id()
                        .map(str::to_string)
                        .unwrap_or_else(|| format!("unit-{}", u.ord));
                    PlanStep {
                        catalog: id.clone(),
                        id,
                        instructions: None,
                        owner: None,
                        depends_on: None,
                        gate: None,
                        added_by: None,
                        floor_reason: None,
                        late: None,
                    }
                })
                .collect(),
            override_: None,
            proposal_id: None,
        }),
    )
}

fn gate_opened_transport(run_id: &str, gate_id: &str, fact: &str, reason: &str) -> TeamEvent {
    event(
        run_id,
        TeamBody::GateOpened(GateOpened {
            gate_id: gate_id.to_string(),
            kind: GateOpenedKind::TeamTransport {
                fact: fact.to_string(),
                reason: reason.to_string(),
            },
        }),
    )
}

fn gate_decided_transport(run_id: &str, gate_id: &str, decision: GateDecision) -> TeamEvent {
    // A team_transport gate names no finding: nothing is unresolved, so it does not team-pause.
    let unresolved: Vec<String> = Vec::new();
    let team_pause = !unresolved.is_empty();
    event(
        run_id,
        TeamBody::GateDecided(GateDecided {
            gate_id: gate_id.to_string(),
            kind: GateKind::TeamTransport,
            decision,
            combined: None,
            team_pause,
            unresolved,
        }),
    )
}

fn token_for(ev: &TeamEvent) -> anyhow::Result<TeamToken> {
    Ok(TeamToken {
        run_id: ev.env.run_id.clone(),
        event_type: ev.event_type().to_string(),
        key: ev.key()?,
    })
}

/// Publish a REQUIRED engine fact: the publisher acknowledges it. `Err(reason)` when there is no
/// publisher to send it to.
fn publish_required(ev: TeamEvent, exhausted: Exhausted) -> Result<TeamToken, String> {
    let token = token_for(&ev).map_err(|e| format!("the fact cannot be keyed: {e:#}"))?;
    send(PublisherReq::Publish {
        event: Box::new(ev),
        ack: Some((token.clone(), exhausted)),
    })?;
    Ok(token)
}

/// Publish a fact nobody waits on (it still rides the FIFO and the outbox).
fn publish_fire(ev: TeamEvent) {
    let _ = send(PublisherReq::Publish {
        event: Box::new(ev),
        ack: None,
    });
}

fn set_unteamed(team: &mut RunTeamState, reason: String) {
    team.transport = Some(Transport::None);
    team.reason = Some(reason);
    team.pending = None;
    team.open_gate = None;
}

/// What the team gate in front of a dispatch decided.
#[derive(Debug)]
pub(super) enum TeamGate {
    /// Dispatch (a non-team run, an un-teamed one, or a teamed run whose path and plan landed).
    Proceed,
    /// A required fact is in flight; the dispatch waits for its acknowledgement.
    Deferred,
    /// The required fact cannot even be sent (the publisher is gone mid-run): pause
    /// `team_transport` with this prompt — never a silent dispatch.
    Pause { prompt: String },
}

/// The team gate in front of a dispatch (called by `advance_or_pause` with a unit at the cursor).
pub(super) fn gate_before_dispatch(
    store: &mut dyn GraphStore,
    session: &mut AgentSession,
    units: &[WorkUnit],
) -> anyhow::Result<TeamGate> {
    #[allow(unreachable_code)] return Ok(TeamGate::Proceed);
    if !is_team_run(session, units) {
        return Ok(TeamGate::Proceed);
    }
    let team = session.team.get_or_insert_with(RunTeamState::default);
    if team.pending.is_some() {
        return Ok(TeamGate::Deferred);
    }
    if team.is_unteamed() {
        return Ok(TeamGate::Proceed);
    }
    if team.transport.is_none() {
        let no_bus = matches!(link(), Some(TeamLink::Unavailable(_)) | None);
        let ev = path_started(session);
        let team = session.team.get_or_insert_with(RunTeamState::default);
        match publish_required(ev, Exhausted::SupersedeRun) {
            Ok(token) => {
                team.pending = Some(PendingTeamFact {
                    event_type: token.event_type,
                    key: token.key,
                    stage: PendingStage::Publishing,
                    then: TeamBlocked::FirstDispatch,
                });
                put_node(store, session.to_node())?;
                return Ok(TeamGate::Deferred);
            }
            Err(reason) => {
                // No bus (§4.8 row 6): nothing is generated or spooled; the run is un-teamed
                // before any team work begins, and says so.
                set_unteamed(team, format!("team transport unavailable: {reason}"));
                team.no_bus = no_bus;
                put_node(store, session.to_node())?;
                return Ok(TeamGate::Proceed);
            }
        }
    }
    if !team.is_teamed() {
        // Transport `bus` without an acknowledged path.started cannot be trusted as teamed.
        set_unteamed(team, "no acknowledged path.started for the run".to_string());
        put_node(store, session.to_node())?;
        return Ok(TeamGate::Proceed);
    }
    if team.plan_rev == Some(PLAN_REV) {
        return Ok(TeamGate::Proceed);
    }
    let ev = plan_accepted(session, units, PLAN_REV);
    let key = ev.key()?;
    let team = session.team.get_or_insert_with(RunTeamState::default);
    let then = TeamBlocked::PlanDispatch { plan_rev: PLAN_REV };
    match publish_required(ev, Exhausted::Keep) {
        Ok(token) => {
            team.pending = Some(PendingTeamFact {
                event_type: token.event_type,
                key: token.key,
                stage: PendingStage::Publishing,
                then,
            });
            put_node(store, session.to_node())?;
            Ok(TeamGate::Deferred)
        }
        Err(reason) => {
            team.gate_seq += 1;
            team.open_gate = Some(tev::gate_id(&session.id, team.gate_seq));
            team.pending = Some(PendingTeamFact {
                event_type: tev::PLAN_ACCEPTED.to_string(),
                key,
                stage: PendingStage::Paused,
                then,
            });
            Ok(TeamGate::Pause {
                prompt: transport_prompt(tev::PLAN_ACCEPTED, &reason),
            })
        }
    }
}

fn transport_prompt(fact: &str, reason: &str) -> String {
    format!(
        "Team transport: `{fact}` could not be published ({reason}). The run is paused before \
         it continues. Approve to retry one more bounded round; approve with amend \
         \"{CONTINUE_WITHOUT_TEAM}\" to run on un-teamed (transport: none); reject to cancel."
    )
}

/// The team snapshot a dispatched unit carries: `transport: none` (with the run's reason) for a
/// team unit of an un-teamed run; `None` otherwise (a teamed attempt's snapshot is T5's).
pub(super) fn unit_snapshot(session: &AgentSession, unit: &WorkUnit) -> Option<UnitTeamSnapshot> {
    #[allow(unreachable_code)] return None;
    let team = session.team.as_ref()?;
    (unit.team_run && team.is_unteamed()).then(|| UnitTeamSnapshot {
        transport: Transport::None,
        reason: team.reason.clone(),
        ledger_source: team.no_bus.then_some(LedgerSource::NoBus),
    })
}

/// Publish `path.ended` for a teamed run reaching a terminal status (not required: a later drain
/// lets consumers forget the run). Nothing for an un-teamed run (§4.8 row 1).
pub(super) fn path_ended(store: &dyn GraphStore, run_id: &str, status: PathStatus) {
    let Ok(Some(session)) = crate::domain::get_session(store, run_id) else {
        return;
    };
    if session.team.as_ref().is_some_and(RunTeamState::is_teamed) {
        publish_fire(event(run_id, TeamBody::PathEnded(PathEnded { status })));
    }
}

/// Everything an acknowledgement handler needs from the actor loop.
pub(super) struct Ctx<'a> {
    pub store: &'a mut dyn GraphStore,
    pub subscribers: &'a mut crate::event_log::EventSink,
    pub runner: &'a Arc<dyn StepRunner>,
    pub self_tx: &'a Sender<Command>,
    pub in_flight: &'a mut HashSet<String>,
    pub lifecycle_maps: &'a Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    pub actor_maps: &'a Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    pub process_gen: uuid::Uuid,
    pub is_acp: bool,
}

/// The pending fact `token` answers, if it is still the one the run waits on.
fn pending_for(
    store: &dyn GraphStore,
    token: &TeamToken,
) -> anyhow::Result<Option<(AgentSession, PendingTeamFact)>> {
    let Some(session) = crate::domain::get_session(store, &token.run_id)? else {
        return Ok(None);
    };
    let Some(p) = session.team.as_ref().and_then(|t| t.pending.clone()) else {
        return Ok(None);
    };
    Ok((p.key == token.key).then_some((session, p)))
}

/// `Command::TeamPublished`: the fact is on the bus — record it and run the step it gated.
/// Returns the run's resulting status when a step ran (`None` for a stale acknowledgement).
pub(super) fn on_published(
    cx: &mut Ctx<'_>,
    token: &TeamToken,
    event_id: i64,
) -> anyhow::Result<Option<SessionStatus>> {
    let Some((mut session, pending)) = pending_for(cx.store, token)? else {
        return Ok(None);
    };
    let team = session.team.get_or_insert_with(RunTeamState::default);
    match token.event_type.as_str() {
        tev::PATH_STARTED => {
            team.transport = Some(Transport::Bus);
            team.stream_floor = Some(event_id);
        }
        tev::GATE_DECIDED => team.open_gate = None,
        _ => {}
    }
    // A plan dispatch gated by a later fact (a team_transport gate's `gate.decided`) still means
    // the plan landed: the lane is FIFO, so nothing after `plan.accepted` lands before it.
    if let TeamBlocked::PlanDispatch { plan_rev } = pending.then {
        team.plan_rev = Some(plan_rev);
    }
    team.pending = None;
    put_node(cx.store, session.to_node())?;
    run_blocked(cx, session, pending.then).map(Some)
}

/// `Command::TeamTransportFailed`: past the bound. `path.started` → un-teamed (the publisher
/// already wrote the tombstone); any other required fact → pause `team_transport`.
pub(super) fn on_failed(
    cx: &mut Ctx<'_>,
    token: &TeamToken,
    reason: &str,
) -> anyhow::Result<Option<SessionStatus>> {
    let Some((mut session, pending)) = pending_for(cx.store, token)? else {
        return Ok(None);
    };
    if token.event_type == tev::PATH_STARTED {
        let team = session.team.get_or_insert_with(RunTeamState::default);
        set_unteamed(team, format!("team transport unavailable: {reason}"));
        put_node(cx.store, session.to_node())?;
        return run_blocked(cx, session, pending.then).map(Some);
    }
    pause_team_transport(cx, session, pending, reason).map(Some)
}

/// Pause the run `team_transport` over `pending` (§4.1): durable pause + open gate row, then a
/// best-effort `gate.opened{team_transport}` that queues behind the missing fact (FIFO).
fn pause_team_transport(
    cx: &mut Ctx<'_>,
    mut session: AgentSession,
    mut pending: PendingTeamFact,
    reason: &str,
) -> anyhow::Result<SessionStatus> {
    let run_id = session.id.clone();
    let units = crate::domain::session_units(cx.store, &run_id)?;
    let ord = units.get(session.unit_ix).map(|u| u.ord).unwrap_or(0);
    let fact = if pending.event_type == tev::GATE_DECIDED {
        // The answer that could not land is a team_transport gate's own; the fact the run is
        // really missing is still the one that gate was opened for.
        match &pending.then {
            TeamBlocked::PlanDispatch { .. } => tev::PLAN_ACCEPTED.to_string(),
            _ => pending.event_type.clone(),
        }
    } else {
        pending.event_type.clone()
    };
    let team = session.team.get_or_insert_with(RunTeamState::default);
    team.gate_seq += 1;
    let gid = tev::gate_id(&run_id, team.gate_seq);
    team.open_gate = Some(gid.clone());
    pending.stage = PendingStage::Paused;
    team.pending = Some(pending.clone());
    let prompt = transport_prompt(&fact, reason);
    pause_for_human(
        cx.store,
        cx.subscribers,
        cx.self_tx,
        &mut session,
        ord,
        None,
        TEAM_TRANSPORT_GATE,
        prompt,
    )?;
    cx.in_flight.remove(&run_id);
    publish_fire(gate_opened_transport(&run_id, &gid, &fact, reason));
    Ok(SessionStatus::AwaitingHuman)
}

/// Whether `session` is paused `team_transport` over a pending fact (its gate is answerable).
pub(super) fn transport_gate_open(session: &AgentSession) -> bool {
    session
        .team
        .as_ref()
        .and_then(|t| t.pending.as_ref())
        .is_some_and(|p| p.stage == PendingStage::Paused)
}

/// Refuse an answer the run cannot take, BEFORE the gate row is resolved: a team fact still in
/// flight, or a `team_transport` gate answered with anything but its three answers.
pub(super) fn refuse_answer(
    session: &AgentSession,
    decision: &crate::workflow::HumanDecision,
) -> anyhow::Result<()> {
    let Some(p) = session.team.as_ref().and_then(|t| t.pending.as_ref()) else {
        return Ok(());
    };
    if p.stage != PendingStage::Paused {
        anyhow::bail!(
            "run {}: team fact `{}` is still being published; answer once it settles",
            session.id,
            p.event_type
        );
    }
    match decision {
        crate::workflow::HumanDecision::RequestChanges { .. } => anyhow::bail!(
            "a team_transport pause takes approve (retry), approve with amend \
             \"{CONTINUE_WITHOUT_TEAM}\", or reject"
        ),
        crate::workflow::HumanDecision::Approve { amend: Some(a), .. }
            if !a.trim().is_empty() && !is_continue_without_team(a) =>
        {
            anyhow::bail!(
                "a team_transport pause takes no amendment other than \"{CONTINUE_WITHOUT_TEAM}\""
            )
        }
        _ => Ok(()),
    }
}

fn is_continue_without_team(a: &str) -> bool {
    a.trim().eq_ignore_ascii_case(CONTINUE_WITHOUT_TEAM)
}

/// Answer a `team_transport` pause (the row is already resolved). Every answer is deferred on
/// the publisher: approve publishes the gate's `gate.decided` (required; FIFO carries the missing
/// fact ahead of it), continue-without-team and reject write the run tombstone first. Returns
/// `AwaitingHuman` — the run's status until the acknowledgement lands.
pub(super) fn answer_transport_gate(
    cx: &mut Ctx<'_>,
    mut session: AgentSession,
    decision: crate::workflow::HumanDecision,
) -> anyhow::Result<SessionStatus> {
    let run_id = session.id.clone();
    let team = session.team.get_or_insert_with(RunTeamState::default);
    let Some(mut pending) = team.pending.clone() else {
        anyhow::bail!("run {run_id} is not paused team_transport");
    };
    let supersede = match &decision {
        crate::workflow::HumanDecision::Reject => {
            pending.then = TeamBlocked::Cancel;
            Some("the operator rejected the team_transport pause")
        }
        crate::workflow::HumanDecision::Approve { amend: Some(a), .. }
            if is_continue_without_team(a) =>
        {
            Some("the operator chose to continue without team")
        }
        _ => None,
    };
    if let Some(reason) = supersede {
        let token = TeamToken {
            run_id: run_id.clone(),
            event_type: pending.event_type.clone(),
            key: pending.key.clone(),
        };
        pending.stage = PendingStage::Superseding;
        team.pending = Some(pending.clone());
        put_node(cx.store, session.to_node())?;
        cx.in_flight.insert(run_id.clone());
        if let Err(e) = send(PublisherReq::SupersedeRun {
            run_id: run_id.clone(),
            from_event: pending.event_type.clone(),
            reason: reason.to_string(),
            ack: Some(token.clone()),
        }) {
            // No publisher to write the tombstone: write it here (the actor never contends —
            // there is no publisher draining), then fall back.
            if let Some(TeamLink::Publisher { outbox, .. }) = link() {
                crate::team::publish::supersede_run_at(
                    &outbox,
                    &run_id,
                    &pending.event_type,
                    reason,
                )?;
            }
            eprintln!("wicked-core: team tombstone for {run_id} written without a publisher: {e}");
            return on_superseded(cx, &token).map(|s| s.unwrap_or(SessionStatus::AwaitingHuman));
        }
        return Ok(SessionStatus::AwaitingHuman);
    }
    // Approve: one more bounded round, carried by this gate's `gate.decided` (required).
    let gid = team
        .open_gate
        .clone()
        .unwrap_or_else(|| tev::gate_id(&run_id, team.gate_seq));
    let ev = gate_decided_transport(&run_id, &gid, GateDecision::HumanApproved);
    match publish_required(ev, Exhausted::Keep) {
        Ok(token) => {
            pending.event_type = token.event_type;
            pending.key = token.key;
            pending.stage = PendingStage::Publishing;
            team.pending = Some(pending);
            put_node(cx.store, session.to_node())?;
            cx.in_flight.insert(run_id);
            Ok(SessionStatus::AwaitingHuman)
        }
        Err(reason) => {
            // Nothing to retry through: still paused, with a fresh gate that says why.
            let session_now = session.clone();
            pause_team_transport(cx, session_now, pending, &reason)
        }
    }
}

/// `Command::TeamSuperseded`: the run tombstone is written — only now persist the fallback, then
/// run the gated step un-teamed (or cancel).
pub(super) fn on_superseded(
    cx: &mut Ctx<'_>,
    token: &TeamToken,
) -> anyhow::Result<Option<SessionStatus>> {
    let Some((mut session, pending)) = pending_for(cx.store, token)? else {
        return Ok(None);
    };
    if pending.stage != PendingStage::Superseding {
        return Ok(None);
    }
    let run_id = session.id.clone();
    if pending.then == TeamBlocked::Cancel {
        // Rejected: the run stays teamed so its `path.ended` is published (the one fact a run
        // tombstone lets through), then it cancels.
        let team = session.team.get_or_insert_with(RunTeamState::default);
        team.pending = None;
        team.open_gate = None;
        put_node(cx.store, session.to_node())?;
        cx.in_flight.remove(&run_id);
        let s = cancel_run(
            cx.store,
            cx.subscribers,
            cx.runner,
            cx.self_tx,
            &run_id,
            cx.lifecycle_maps,
        )?;
        return Ok(Some(s));
    }
    let team = session.team.get_or_insert_with(RunTeamState::default);
    set_unteamed(
        team,
        "the operator chose to continue without team".to_string(),
    );
    put_node(cx.store, session.to_node())?;
    run_blocked(cx, session, pending.then).map(Some)
}

/// Run the step a pending fact gated.
fn run_blocked(
    cx: &mut Ctx<'_>,
    mut session: AgentSession,
    then: TeamBlocked,
) -> anyhow::Result<SessionStatus> {
    let run_id = session.id.clone();
    match then {
        TeamBlocked::Cancel => {
            cx.in_flight.remove(&run_id);
            cancel_run(
                cx.store,
                cx.subscribers,
                cx.runner,
                cx.self_tx,
                &run_id,
                cx.lifecycle_maps,
            )
        }
        TeamBlocked::FirstDispatch | TeamBlocked::PlanDispatch { .. } => {
            if session.status == SessionStatus::AwaitingHuman {
                // The team_transport gate was answered and its step can now run: resume.
                session.status = SessionStatus::Executing;
                put_node(cx.store, session.to_node())?;
                let units = crate::domain::session_units(cx.store, &run_id)?;
                let ord = units.get(session.unit_ix).map(|u| u.ord).unwrap_or(0);
                emit(
                    cx.subscribers,
                    CoreEvent::Resumed {
                        session: run_id.clone(),
                        ord,
                    },
                );
            }
            let progress = advance_or_pause(
                cx.store,
                cx.subscribers,
                cx.runner,
                cx.self_tx,
                &run_id,
                session.unit_ix,
                cx.lifecycle_maps,
                cx.actor_maps,
                cx.process_gen,
                cx.is_acp,
            );
            match progress {
                Ok(Progress::Dispatched) | Ok(Progress::Deferred) => {
                    cx.in_flight.insert(run_id);
                    Ok(SessionStatus::Executing)
                }
                Ok(Progress::Paused) => {
                    cx.in_flight.remove(&run_id);
                    Ok(SessionStatus::AwaitingHuman)
                }
                Ok(Progress::Done) => {
                    cx.in_flight.remove(&run_id);
                    finalize_run(cx.store, cx.subscribers, cx.runner, cx.self_tx, &run_id)?;
                    Ok(SessionStatus::Completed)
                }
                Err(e) => {
                    cx.in_flight.remove(&run_id);
                    fail_run_by_id(
                        cx.store,
                        cx.subscribers,
                        cx.runner,
                        cx.self_tx,
                        &run_id,
                        anyhow::anyhow!("{e:#}"),
                    );
                    Err(e)
                }
            }
        }
    }
}

/// Boot reconcile (§4.1 "crash between tombstone and store write", §4.8 row 12): a LIVE team run
/// with no acknowledged `path.started` is un-teamed — its run tombstone is written FIRST, then
/// the store — and a run caught mid-fact re-opens its `team_transport` pause so a human decides
/// again. Runs before the publisher is asked to drain, so no drain can race the tombstone.
pub(super) fn reconcile_at_boot(store: &mut dyn GraphStore) {
        #[allow(unreachable_code)] return;
    let Ok(sessions) = crate::domain::all_sessions(store) else {
        return;
    };
    let outbox = match link() {
        Some(TeamLink::Publisher { outbox, .. }) => Some(outbox),
        _ => None,
    };
    for mut session in sessions {
        if !matches!(
            session.status,
            SessionStatus::Planning
                | SessionStatus::Distributing
                | SessionStatus::Executing
                | SessionStatus::AwaitingHuman
        ) {
            continue;
        }
        let units = crate::domain::session_units(store, &session.id).unwrap_or_default();
        if !is_team_run(&session, &units) {
            continue;
        }
        let run_id = session.id.clone();
        let team = session.team.clone().unwrap_or_default();
        let unteamed_already = team.is_unteamed();
        if !unteamed_already && team.stream_floor.is_none() {
            // Tombstone first (whatever the outbox holds for the run), then the store.
            if let Some(outbox) = &outbox {
                if let Err(e) = crate::team::publish::supersede_run_at(
                    outbox,
                    &run_id,
                    tev::PATH_STARTED,
                    "boot: no acknowledged path.started",
                ) {
                    eprintln!(
                        "wicked-core: boot could not tombstone team run {run_id} ({e}); leaving \
                         it undecided"
                    );
                    continue;
                }
            }
            let t = session.team.get_or_insert_with(RunTeamState::default);
            set_unteamed(
                t,
                "the daemon restarted before the run's path.started was acknowledged".to_string(),
            );
            let _ = put_node(store, session.to_node());
            continue;
        }
        let Some(pending) = team.pending.clone() else {
            continue;
        };
        match pending.stage {
            PendingStage::Paused => {}
            PendingStage::Publishing | PendingStage::Superseding => {
                if pending.stage == PendingStage::Superseding {
                    // The answer was recorded; its tombstone may not be. Write it, then leave
                    // the decision to a human again: the run is un-teamed from here.
                    if let Some(outbox) = &outbox {
                        let _ = crate::team::publish::supersede_run_at(
                            outbox,
                            &run_id,
                            &pending.event_type,
                            "boot: an answer was being applied at restart",
                        );
                    }
                    let t = session.team.get_or_insert_with(RunTeamState::default);
                    set_unteamed(
                        t,
                        "the daemon restarted while a team_transport answer was applied"
                            .to_string(),
                    );
                } else {
                    let t = session.team.get_or_insert_with(RunTeamState::default);
                    t.gate_seq += 1;
                    t.open_gate = Some(tev::gate_id(&run_id, t.gate_seq));
                    t.pending = Some(PendingTeamFact {
                        stage: PendingStage::Paused,
                        ..pending.clone()
                    });
                }
                let ord = units.get(session.unit_ix).map(|u| u.ord).unwrap_or(0);
                session.status = SessionStatus::AwaitingHuman;
                let prompt = format!(
                    "Team transport: the daemon restarted while `{}` was in flight. Approve to \
                     continue, approve with amend \"{CONTINUE_WITHOUT_TEAM}\" to run un-teamed, \
                     or reject to cancel.",
                    pending.event_type
                );
                let request = crate::interaction::open_gate(
                    &run_id,
                    ord,
                    None,
                    &prompt,
                    TEAM_TRANSPORT_GATE,
                    crate::interaction::now_millis(),
                );
                let _ = crate::domain::put_nodes(store, &[session.to_node(), request.to_node()]);
            }
        }
    }
}

/// Tell the publisher to drain the outbox once (after the boot reconcile).
pub(super) fn drain_at_boot() {
    let _ = send(PublisherReq::DrainAll);
}

/// Whether `run_id`'s last answer is still waiting on the publisher (a held `confirm_gate` reply).
pub(super) fn answer_in_flight(store: &dyn GraphStore, run_id: &str) -> bool {
    crate::domain::get_session(store, run_id)
        .ok()
        .flatten()
        .and_then(|s| s.team)
        .and_then(|t| t.pending)
        .is_some_and(|p| {
            matches!(
                p.stage,
                PendingStage::Publishing | PendingStage::Superseding
            )
        })
}

/// Answer a held `confirm_gate` reply once the run's step ran — unless the run is waiting on a
/// further fact (the reply then keeps waiting for that one).
pub(super) fn settle_reply(
    store: &dyn GraphStore,
    replies: &mut HashMap<String, Sender<anyhow::Result<SessionStatus>>>,
    run_id: &str,
    res: anyhow::Result<Option<SessionStatus>>,
) {
    match res {
        Ok(None) => {}
        Ok(Some(status)) => {
            if !answer_in_flight(store, run_id) {
                if let Some(reply) = replies.remove(run_id) {
                    let _ = reply.send(Ok(status));
                }
            }
        }
        Err(e) => {
            eprintln!("wicked-core: team acknowledgement for {run_id} failed: {e:#}");
            if let Some(reply) = replies.remove(run_id) {
                let _ = reply.send(Err(e));
            }
        }
    }
}

#[cfg(test)]
#[path = "team_gate_tests.rs"]
mod tests;
