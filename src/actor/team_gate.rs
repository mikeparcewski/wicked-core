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
use std::path::PathBuf;

use super::*;
use crate::domain::{
    PendingStage, PendingTeamFact, RunTeamState, TeamBlocked, UnitTeamSnapshot, WorkUnit,
};
use crate::team::events::{
    self as tev, Envelope, GateDecided, GateDecision, GateKind, GateOpened, GateOpenedKind,
    LedgerSource, PathEnded, PathStarted, PathStatus, PlanAccepted, PlanMode, PlanStep, Selection,
    TeamBody, TeamEvent, Transport,
};
use crate::team::publish::{Exhausted, PublisherReq, TeamBus, TeamLink, TeamToken};

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
        Some(TeamLink::Unavailable { reason, .. }) => Err(reason),
        None => Err("no team publisher on this engine".to_string()),
    }
}

/// Whether THIS process can publish team facts (a publisher thread is linked). A teamed run is
/// live-teamed only while this holds (bus present at launch, absent now = not teamed here).
pub(super) fn has_publisher() -> bool {
    matches!(link(), Some(TeamLink::Publisher { .. }))
}

/// (T5) The attempt runner's handle on team publishing, for the worker threads this actor starts
/// (in-process and the bus worker). `None` without a publisher: a teamed unit's worker then runs
/// the attempt un-teamed and says so ([`crate::team::runner::claim`]).
pub(super) fn team_runner() -> Option<crate::team::runner::TeamRunner> {
    match link() {
        Some(TeamLink::Publisher { runner, .. }) => Some(*runner),
        _ => None,
    }
}

/// The pending "fact" of a teamed run paused because this process has no publisher for it.
pub const TRANSPORT_UNAVAILABLE: &str = "team_transport_unavailable";

/// The paused pending fact for a teamed run this process cannot publish for: answered by the
/// team handler like any `team_transport` pause (continue → tombstone, then `transport: none`;
/// reject → tombstone, cancel; approve → retry through the gate's `gate.decided`).
fn unavailable_pending() -> PendingTeamFact {
    PendingTeamFact {
        event_type: TRANSPORT_UNAVAILABLE.to_string(),
        key: String::new(),
        stage: PendingStage::Paused,
        then: TeamBlocked::Continue,
        staged: None,
    }
}

/// The team outbox — a property of the STATE HOME, not of bus availability (review of #623
/// round 5): a daemon with no bus still owes the tombstones of runs it finishes.
fn outbox_path() -> Option<PathBuf> {
    match link() {
        Some(TeamLink::Publisher { outbox, .. }) => Some(outbox),
        Some(TeamLink::Unavailable { outbox, .. }) => outbox,
        None => None,
    }
}

/// Write `run_id`'s run tombstone into the outbox synchronously — for the paths that have no
/// publisher to write it (no bus; the publisher stopped) and for the boot reconcile, which runs
/// before any drain. `Err` when there is no outbox path or the write fails: the caller then
/// FAILS CLOSED (keeps the pause), never proceeds as if the tombstone existed.
fn write_tombstone(run_id: &str, from_event: &str, reason: &str) -> Result<(), String> {
    let outbox = outbox_path().ok_or_else(|| "no team outbox (no state home)".to_string())?;
    crate::team::publish::supersede_run_at(&outbox, run_id, from_event, reason)
        .map_err(|e| format!("the run tombstone could not be written: {e}"))
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
            // (T3, codex round 7) What the launch named, from its durable plan state: the preset,
            // or a user-composed plan. A run on a bare composed def names neither.
            workflow: session.team_plan.as_ref().and_then(|t| t.preset.clone()),
            plan: session
                .team_plan
                .as_ref()
                .is_some_and(|t| t.preset.is_none()),
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
        let no_bus = matches!(link(), Some(TeamLink::Unavailable { .. }) | None);
        let ev = path_started(session);
        let team = session.team.get_or_insert_with(RunTeamState::default);
        match publish_required(ev, Exhausted::SupersedeRun) {
            Ok(token) => {
                team.pending = Some(PendingTeamFact {
                    event_type: token.event_type,
                    key: token.key,
                    stage: PendingStage::Publishing,
                    then: TeamBlocked::FirstDispatch,
                    staged: None,
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
    if !has_publisher() {
        // Teamed, but this process cannot publish its required facts (bus present at launch,
        // absent now): a required-transport failure — pause, never dispatch as if teamed.
        team.gate_seq += 1;
        team.open_gate = Some(tev::gate_id(&session.id, team.gate_seq));
        team.pending = Some(unavailable_pending());
        return Ok(TeamGate::Pause {
            prompt: transport_prompt(
                TRANSPORT_UNAVAILABLE,
                "this daemon has no team publisher (no bus)",
            ),
        });
    }
    // DES-TEAMING-002 T3: the facts the plan pipeline built before the path landed (the launch's
    // `plan.proposed` / `path.scored`) follow `path.started` onto the bus — never ahead of it.
    if let Some(tp) = session.team_plan.as_mut() {
        if !tp.queued.is_empty() {
            for fact in crate::plan_gate::take_queued(tp) {
                publish_fire(fact);
            }
            put_node(store, session.to_node())?;
        }
    }
    // T3: a plan still held for approval has no accepted rev: nothing to accept yet — the
    // `plan_approval` pause (`should_pause`) comes next, and its answer accepts the rev.
    let accepted = match session.team_plan.as_ref() {
        Some(tp) => match tp.accepted.clone() {
            Some(a) => Some(a),
            None => return Ok(TeamGate::Proceed),
        },
        None => None,
    };
    let plan_rev = accepted.as_ref().map_or(PLAN_REV, |a| a.rev);
    let team = session.team.get_or_insert_with(RunTeamState::default);
    if team.plan_rev == Some(plan_rev) {
        return Ok(TeamGate::Proceed);
    }
    // T3 fills `plan.accepted` from the accepted plan (band, high_risk, steps, by, override); a
    // team run launched on a bare composed def (no plan state) keeps P1's unit-derived body.
    let ev = match &accepted {
        Some(a) => {
            crate::plan_gate::plan_accepted(&session.id, a, crate::interaction::now_millis())?
        }
        None => plan_accepted(session, units, PLAN_REV),
    };
    let key = ev.key()?;
    let team = session.team.get_or_insert_with(RunTeamState::default);
    let then = TeamBlocked::PlanDispatch { plan_rev };
    match publish_required(ev, Exhausted::Keep) {
        Ok(token) => {
            team.pending = Some(PendingTeamFact {
                event_type: token.event_type,
                key: token.key,
                stage: PendingStage::Publishing,
                then,
                staged: None,
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
                staged: None,
            });
            Ok(TeamGate::Pause {
                prompt: transport_prompt(tev::PLAN_ACCEPTED, &reason),
            })
        }
    }
}

/// (DES-TEAMING-002 T3) Why no unit of this run may dispatch now — the ONE dispatch guard,
/// enforced inside `dispatch_unit` (every path that runs a unit goes through it: the advance, a
/// redrive, a reassign, a rework): a plan held for approval. Nothing is released until its
/// `plan_approval` gate is answered. `None` for a run the plan pipeline does not hold.
pub(super) fn dispatch_blocked(session: &AgentSession) -> Option<String> {
    let p = session.team_plan.as_ref()?.pending.as_ref()?;
    Some(format!(
        "run {} holds plan rev {} for approval: no unit dispatches until its plan_approval gate \
         is answered",
        session.id, p.rev
    ))
}

/// A dispatch refused by [`dispatch_blocked`]: typed, so a caller that can route the run to the
/// gate that holds it (a restart redrive) tells it from a dispatch fault.
#[derive(Debug)]
pub(crate) struct DispatchHeld(pub String);

impl std::fmt::Display for DispatchHeld {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for DispatchHeld {}

// ── T3: the plan_approval gate on P1's path (DES-TEAMING-002 §8.6) ─────────────────────────────

/// Whether this run's team facts go on the bus: teamed (acknowledged `path.started`) with a
/// publisher in this process. An un-teamed run publishes nothing; its plan gate works the same.
pub(super) fn publishes(session: &AgentSession) -> bool {
    session.team.as_ref().is_some_and(RunTeamState::is_teamed) && has_publisher()
}

/// Publish plan-gate facts nobody waits on (`gate.opened{plan_approval}`, a refused edit's
/// `plan.proposed` / `path.scored` / `plan.refused`, a reject's `gate.decided`) — FIFO, outbox.
pub(super) fn publish_plan_facts(session: &AgentSession, facts: Vec<TeamEvent>) {
    if publishes(session) {
        for f in facts {
            publish_fire(f);
        }
    }
}

/// The plan gate's gate id, minted from the run's ONE gate counter ([`RunTeamState::gate_seq`]) in
/// the same batch as the pause (the caller persists `session`).
pub(super) fn mint_gate_id(session: &mut AgentSession) -> String {
    let run_id = session.id.clone();
    let team = session.team.get_or_insert_with(RunTeamState::default);
    team.gate_seq += 1;
    tev::gate_id(&run_id, team.gate_seq)
}

/// A `plan_approval` gate opened: `gate.opened` rides the FIFO behind the pause, and the gate is
/// the run's open team gate until its `gate.decided` lands (P1 `open_gate`).
pub(super) fn plan_gate_opened(
    store: &mut dyn GraphStore,
    session: &mut AgentSession,
    gate_id: &str,
    opened: TeamEvent,
) -> anyhow::Result<()> {
    if !publishes(session) {
        return Ok(());
    }
    if let Some(team) = session.team.as_mut() {
        team.open_gate = Some(gate_id.to_string());
    }
    put_node(store, session.to_node())?;
    publish_fire(opened);
    Ok(())
}

/// Release the run past an answered `plan_approval` gate (approve, an edit accepted as the next
/// rev, or a refused edit that re-opens it). Teamed: the gate's `gate.decided` is a REQUIRED
/// transition published FIRST, with the staged answer held durably on its pending fact; the answer
/// applies only on the acknowledgement ([`on_published`] → [`finish_release`]), and a failure past
/// the bound pauses `team_transport` with nothing applied (P1). Un-teamed: nothing to wait on —
/// the answer applies now, through the same [`finish_release`]. Either way the run advances
/// through `advance_or_pause`, so the cursor unit is dispatched once, at its current attempt.
pub(super) fn release_plan_gate(
    cx: &mut Ctx<'_>,
    mut session: AgentSession,
    decided: TeamEvent,
    release: crate::plan_gate::StagedRelease,
) -> anyhow::Result<SessionStatus> {
    let run_id = session.id.clone();
    if publishes(&session) {
        let then = TeamBlocked::Continue;
        let key = decided.key()?;
        let staged = Some(Box::new(release));
        match publish_required(decided, Exhausted::Keep) {
            Ok(token) => {
                let team = session.team.get_or_insert_with(RunTeamState::default);
                team.pending = Some(PendingTeamFact {
                    event_type: token.event_type,
                    key: token.key,
                    stage: PendingStage::Publishing,
                    then,
                    staged,
                });
                put_node(cx.store, session.to_node())?;
                cx.in_flight.insert(run_id);
                return Ok(SessionStatus::AwaitingHuman);
            }
            Err(reason) => {
                let pending = PendingTeamFact {
                    event_type: tev::GATE_DECIDED.to_string(),
                    key,
                    stage: PendingStage::Paused,
                    then,
                    staged,
                };
                return pause_team_transport(cx, session, pending, &reason);
            }
        }
    }
    finish_release(cx, session, release)
}

/// Apply a staged plan-gate answer ([`crate::plan_gate::StagedRelease`]) to the run's durable
/// state: the plan state it commits (with the facts that follow `gate.decided` queued on it — a
/// teamed run publishes them ahead of its next `plan.accepted` / `gate.opened`, an un-teamed one
/// publishes nothing), the unit swap onto the proven def (new units written before the old go),
/// the released cursor unit, and — in the same last write — the `Acknowledged` pending fact
/// cleared. IDEMPOTENT, so a restart part-way repeats it safely: the state is absolute (never
/// incremented), the units are keyed by phase id, and a queued fact is published once, by the
/// dispatch that takes it.
pub(super) fn apply_release(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    mut session: AgentSession,
    release: crate::plan_gate::StagedRelease,
) -> anyhow::Result<AgentSession> {
    let run_id = session.id.clone();
    let mut next = release.state;
    if session.team.as_ref().is_some_and(RunTeamState::is_teamed) {
        next.queued.extend(release.facts);
    }
    session.team_plan = Some(next);
    put_node(store, session.to_node())?;
    if let Some(def) = release.def {
        super::replan_for_accepted_edit(store, subscribers, &session, def.into_def())?;
        session = crate::domain::get_session(&*store, &run_id)?
            .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    }
    if !release.reopen {
        // The cursor unit — which never ran — is released once, at its current attempt.
        let cursor_ord = crate::domain::session_units(store, &run_id)?
            .get(session.unit_ix)
            .map(|u| u.ord);
        if let Some(tp) = session.team_plan.as_mut() {
            tp.released_ord = cursor_ord;
        }
    }
    if let Some(team) = session.team.as_mut() {
        if release.reopen {
            team.open_gate = None;
        }
        if team
            .pending
            .as_ref()
            .is_some_and(|p| p.stage == PendingStage::Acknowledged)
        {
            team.pending = None;
        }
    }
    put_node(store, session.to_node())?;
    Ok(session)
}

/// Apply a staged answer, then take the step it leads to: release the run (resume + dispatch
/// through `advance_or_pause`), or re-open its plan gate (a refused edit). A failed apply fails
/// the run — never a run left waiting on an answer that cannot apply.
fn finish_release(
    cx: &mut Ctx<'_>,
    session: AgentSession,
    release: crate::plan_gate::StagedRelease,
) -> anyhow::Result<SessionStatus> {
    let run_id = session.id.clone();
    let reopen = release.reopen;
    let session = match apply_release(cx.store, cx.subscribers, session, release) {
        Ok(s) => s,
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
            return Err(e);
        }
    };
    if !reopen {
        // (T3 round 10) The run was paused at its plan gate: say it resumed. `run_blocked` says so
        // for a run still `AwaitingHuman`; an accepted edit's re-plan already wrote `Executing`.
        if session.status != SessionStatus::AwaitingHuman {
            let ord = crate::domain::session_units(cx.store, &run_id)?
                .get(session.unit_ix)
                .map(|u| u.ord)
                .unwrap_or(0);
            emit(
                cx.subscribers,
                CoreEvent::Resumed {
                    session: run_id.clone(),
                    ord,
                },
            );
        }
        return run_blocked(cx, session, TeamBlocked::Continue);
    }
    match advance_or_pause(
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
    )? {
        Progress::Paused => {
            cx.in_flight.remove(&run_id);
            Ok(SessionStatus::AwaitingHuman)
        }
        Progress::Dispatched | Progress::Done | Progress::Deferred => {
            anyhow::bail!("run {run_id}: a refused plan edit must re-open its gate")
        }
    }
}

/// The staged answer of `pending`, now committing: the pending fact is recorded `Acknowledged`
/// (durably, before anything applies — a restart finishes the apply, never re-asks).
fn acknowledge_staged(
    store: &mut dyn GraphStore,
    session: &mut AgentSession,
    pending: &PendingTeamFact,
) -> anyhow::Result<Option<crate::plan_gate::StagedRelease>> {
    let Some(staged) = pending.staged.clone() else {
        return Ok(None);
    };
    let team = session.team.get_or_insert_with(RunTeamState::default);
    team.pending = Some(PendingTeamFact {
        stage: PendingStage::Acknowledged,
        ..pending.clone()
    });
    put_node(store, session.to_node())?;
    Ok(Some(*staged))
}

/// Boot: finish a staged plan-gate answer whose `gate.decided` was acknowledged (or whose
/// operator continued without team) before the restart — applied once ([`apply_release`] is
/// idempotent). A release leaves the run `Executing`, resumed like every run a restart
/// interrupts (`resume_run`); a refused edit re-opens its gate now.
#[allow(clippy::too_many_arguments)]
pub(super) fn finish_release_at_boot(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    runner: &Arc<dyn StepRunner>,
    self_tx: &Sender<Command>,
    lifecycle_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    actor_maps: &Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    process_gen: uuid::Uuid,
    is_acp: bool,
    run_id: &str,
) -> anyhow::Result<()> {
    let Some(session) = crate::domain::get_session(&*store, run_id)? else {
        return Ok(());
    };
    let Some(staged) = session
        .team
        .as_ref()
        .and_then(|t| t.pending.as_ref())
        .filter(|p| p.stage == PendingStage::Acknowledged)
        .and_then(|p| p.staged.clone())
    else {
        return Ok(());
    };
    let reopen = staged.reopen;
    let mut session = apply_release(store, subscribers, session, *staged)?;
    if reopen {
        let unit_ix = session.unit_ix;
        advance_or_pause(
            store,
            subscribers,
            runner,
            self_tx,
            run_id,
            unit_ix,
            lifecycle_maps,
            actor_maps,
            process_gen,
            is_acp,
        )?;
    } else {
        session.status = SessionStatus::Executing;
        put_node(store, session.to_node())?;
    }
    Ok(())
}

fn transport_prompt(fact: &str, reason: &str) -> String {
    format!(
        "Team transport: `{fact}` could not be published ({reason}). The run is paused before \
         it continues. Approve to retry one more bounded round; approve with amend \
         \"{CONTINUE_WITHOUT_TEAM}\" to run on un-teamed (transport: none); reject to cancel."
    )
}

/// The team snapshot a dispatched team unit carries, stamped from the run's persisted state
/// before its turn starts: `transport: none` (with the run's reason) for an un-teamed run; for a
/// teamed run (T5) `transport: bus` with the run's `stream_floor`, which the worker's
/// `step.claimed` confirms or downgrades. `None` for a non-team unit.
///
/// A team unit of a run whose transport is still undecided is never dispatched (P1 defers it);
/// should one reach here it is stamped `none` with the reason, so it can never run teamed on no
/// evidence nor un-teamed in silence.
pub(super) fn unit_snapshot(session: &AgentSession, unit: &WorkUnit) -> Option<UnitTeamSnapshot> {
    if !unit.team_run {
        return None;
    }
    let Some(team) = session.team.as_ref() else {
        return Some(UnitTeamSnapshot::stamped(
            Transport::None,
            Some("no team state at dispatch".into()),
            None,
        ));
    };
    if team.is_unteamed() {
        return Some(UnitTeamSnapshot::stamped(
            Transport::None,
            team.reason.clone(),
            team.no_bus.then_some(LedgerSource::NoBus),
        ));
    }
    if unit.tool_cmd.is_some() {
        // The engine's own command: no seat takes a step, so nothing brackets it on the bus.
        return Some(UnitTeamSnapshot::stamped(
            Transport::None,
            Some("tool unit: the engine's own command is not a team step".into()),
            None,
        ));
    }
    if team.is_teamed() {
        let mut s = UnitTeamSnapshot::stamped(Transport::Bus, None, None);
        s.stream_floor = team.stream_floor;
        return Some(s);
    }
    Some(UnitTeamSnapshot::stamped(
        Transport::None,
        Some("team transport undecided at dispatch".into()),
        None,
    ))
}

/// The `gate_kind` of the pause a teamed unit's ledger opens when the gate approved it but an
/// unresolved HIGH (or an incomplete record) stands without a council YES (DES-001 §6.7, T5 (c)).
pub const TEAM_DISPUTE_GATE: &str = "team_dispute";

fn unit_event(run_id: &str, ord: u32, attempt: u32, body: TeamBody) -> TeamEvent {
    TeamEvent {
        env: Envelope {
            ord: Some(ord),
            attempt: Some(attempt),
            ..envelope(run_id)
        },
        body,
    }
}

/// (T5, §8.11) Before the fold: merge the unit's dispatch stamp with the worker's snapshot
/// ([`crate::team::runner::merge_snapshot`]) onto `unit.team` (the fold persists it), and for a
/// teamed attempt mint the unit-review gate and publish `gate.opened{kind:"unit_review"}` with
/// the S row the gate read (`ledger_ref`), or `null` + `synthesized` for the worker's own
/// fail-closed ledger. Returns the gate id, `None` for a non-team or un-teamed unit (§4.8 rows
/// 1/5/6: no team fact is generated for it).
pub(super) fn open_unit_review(
    session: &mut AgentSession,
    unit: &mut WorkUnit,
    attempt: u32,
    worker: Option<UnitTeamSnapshot>,
) -> Option<String> {
    if !unit.team_run {
        return None;
    }
    unit.team = crate::team::runner::merge_snapshot(unit.team.as_ref(), worker);
    let snap = unit.team.as_ref()?;
    if snap.transport != Transport::Bus {
        return None;
    }
    let source = snap.ledger_source.unwrap_or(LedgerSource::Synthesized);
    let ledger_ref = snap
        .ledger_ref
        .clone()
        .filter(|_| source == LedgerSource::Folded);
    let team = session.team.get_or_insert_with(Default::default);
    team.gate_seq += 1;
    let gid = tev::gate_id(&session.id, team.gate_seq);
    publish_fire(unit_event(
        &session.id,
        unit.ord,
        attempt,
        TeamBody::GateOpened(GateOpened {
            gate_id: gid.clone(),
            kind: GateOpenedKind::UnitReview {
                ledger_ref,
                ledger_source: source,
            },
        }),
    ));
    Some(gid)
}

/// A `team_dispute` the fold decided the unit opens: the prompt and the unresolved HIGHs it names.
#[derive(Debug, Clone)]
pub(super) struct DisputeOpen {
    pub prompt: String,
    pub finding_ids: Vec<String>,
}

/// (T5) After the fold: publish the unit-review `gate.decided`, and when the gate APPROVED a
/// teamed unit whose ledger pauses ([`tev::gate_pauses`]) return the `team_dispute` the caller
/// opens ([`open_dispute`]): the unit's work stands, the run does not continue unattended. A
/// denied unit is denied, never paused.
pub(super) fn decide_unit_review(
    session: &mut AgentSession,
    unit: &WorkUnit,
    attempt: u32,
    gate_id: &str,
    approved: bool,
) -> Option<DisputeOpen> {
    // A teamed unit whose snapshot carries no ledger is an incomplete record: it pauses.
    let owned;
    let ledger = match unit.team.as_ref()?.ledger.as_ref() {
        Some(l) => l,
        None => {
            owned = crate::team::TeamLedger::new(
                crate::team::FinalPass::StreamGap,
                Vec::new(),
                Vec::new(),
                Default::default(),
            );
            &owned
        }
    };
    let pause = tev::gate_pauses(approved, ledger);
    let unresolved: Vec<String> = tev::unresolved_highs(ledger)
        .iter()
        .map(|f| f.finding.finding_id.clone())
        .collect();
    publish_fire(unit_event(
        &session.id,
        unit.ord,
        attempt,
        TeamBody::GateDecided(GateDecided {
            gate_id: gate_id.to_string(),
            kind: GateKind::UnitReview,
            decision: if !approved {
                GateDecision::Deny
            } else if pause {
                GateDecision::Paused
            } else {
                GateDecision::Allow
            },
            combined: Some(approved),
            team_pause: ledger.team_pause,
            unresolved: unresolved.clone(),
        }),
    ));
    if !pause {
        return None;
    }
    let why = if unresolved.is_empty() {
        format!(
            "the team's record of this step is incomplete (final pass: {})",
            serde_json::to_value(ledger.final_pass)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        )
    } else {
        format!(
            "unresolved HIGH finding(s) without a council YES: {}",
            unresolved.join(", ")
        )
    };
    Some(DisputeOpen {
        prompt: format!(
            "Team dispute on unit {} ({}): {why}. The gate approved the work, but the run does \
             not continue unattended. Approve to continue, request changes to rework, or reject \
             to cancel.",
            unit.ord, unit.description
        ),
        finding_ids: unresolved,
    })
}

/// A team run reached a terminal status (called right AFTER the terminal status is persisted).
/// Teamed: publish `path.ended` so consumers forget the run. Still deciding (its `path.started`
/// in flight): tombstone the run's lines, so a late landing never opens a path that will not end.
/// Un-teamed: nothing (§4.8 row 1). Both requests are acknowledged, and the acknowledgement marks
/// the end on record (`RunTeamState.ended`); until it does, boot reconcile re-issues the same end
/// ([`end_terminal_run`]) — the durable status drives it, and the actor never waits on the bus.
pub(super) fn run_ended(store: &dyn GraphStore, run_id: &str, status: PathStatus) {
    let Ok(Some(session)) = crate::domain::get_session(store, run_id) else {
        return;
    };
    if let Some(team) = session.team.as_ref() {
        request_end(run_id, team, status);
    }
}

fn request_end(run_id: &str, team: &RunTeamState, status: PathStatus) {
    if team.ended || team.is_unteamed() {
        return;
    }
    // With no publisher to take the request, the end is written into the outbox directly (the
    // actor has no publisher to contend with). If even that fails, `ended` stays unset and the
    // next boot re-issues the end — never skipped silently.
    if team.is_teamed() {
        let ev = event(run_id, TeamBody::PathEnded(PathEnded { status }));
        if let Err(why) = publish_required(ev.clone(), Exhausted::Keep) {
            let spooled = outbox_path()
                .ok_or_else(|| "no team outbox (no state home)".to_string())
                .and_then(|o| {
                    TeamBus::new("", o, crate::team::publish::ATTEMPT_WAIT)
                        .spool_only(&ev, &format!("no publisher: {why}"))
                        .map_err(|e| format!("{e:#}"))
                });
            if let Err(e) = spooled {
                eprintln!("wicked-core: path.ended for {run_id} not recorded ({e}); boot retries");
            }
        }
    } else {
        let reason = format!(
            "the run ended ({}) before its path.started landed",
            status.as_str()
        );
        let sent = send(PublisherReq::SupersedeRun {
            run_id: run_id.to_string(),
            from_event: tev::PATH_STARTED.to_string(),
            reason: reason.clone(),
            ack: Some(TeamToken {
                run_id: run_id.to_string(),
                event_type: tev::PATH_STARTED.to_string(),
                key: String::new(),
            }),
        });
        if sent.is_err() {
            if let Err(e) = write_tombstone(run_id, tev::PATH_STARTED, &reason) {
                eprintln!(
                    "wicked-core: tombstone for ended run {run_id} not written ({e}); boot retries"
                );
            }
        }
    }
}

fn path_status(s: SessionStatus) -> Option<PathStatus> {
    Some(match s {
        SessionStatus::Completed => PathStatus::Completed,
        SessionStatus::Cancelled => PathStatus::Cancelled,
        SessionStatus::Failed => PathStatus::Failed,
        _ => return None,
    })
}

/// Record a terminal run's end as acknowledged (`path.ended` landed, or its tombstone written).
fn mark_ended(store: &mut dyn GraphStore, run_id: &str) -> anyhow::Result<()> {
    let Some(mut session) = crate::domain::get_session(store, run_id)? else {
        return Ok(());
    };
    if path_status(session.status).is_none() {
        return Ok(());
    }
    if let Some(team) = session.team.as_mut() {
        if !team.ended {
            team.ended = true;
            put_node(store, session.to_node())?;
        }
    }
    Ok(())
}

/// Boot reconcile for a TERMINAL team run whose end is not on record (a crash between the
/// terminal `put_node` and the publisher handling `run_ended`'s request): re-issue exactly that
/// end. A run that never acknowledged its `path.started` is tombstoned HERE, synchronously and
/// before the boot drain (so the drain cannot publish its spooled `path.started`), and marked
/// ended; a teamed run's `path.ended` is re-requested (same key, so a line already spooled or a
/// row already on the bus is not duplicated) and marked ended by its acknowledgement.
fn end_terminal_run(store: &mut dyn GraphStore, session: &mut AgentSession) -> bool {
    let Some(status) = path_status(session.status) else {
        return true;
    };
    let run_id = session.id.clone();
    let Some(team) = session.team.as_mut() else {
        return true;
    };
    if team.ended || team.is_unteamed() {
        return true;
    }
    if team.is_teamed() {
        request_end(&run_id, team, status);
        return true;
    }
    // The tombstone is a precondition of "ended": no outbox path or a failed write leaves the
    // run un-ended and out of the boot drain (fail closed); the next boot retries.
    if let Err(e) = write_tombstone(
        &run_id,
        tev::PATH_STARTED,
        "boot: the run ended before its path.started landed",
    ) {
        eprintln!("wicked-core: boot could not tombstone ended team run {run_id}: {e}");
        return false;
    }
    team.ended = true;
    if let Err(e) = put_node(store, session.to_node()) {
        // The tombstone is written (the durable fact); only the marker is lost — the next boot
        // writes the same tombstone again and marks it then.
        eprintln!("wicked-core: boot could not mark team run {run_id} ended: {e}");
    }
    true
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
    // A run that ended meanwhile (cancelled while its fact was in flight) runs no gated step.
    if matches!(
        session.status,
        SessionStatus::Completed | SessionStatus::Cancelled | SessionStatus::Failed
    ) {
        return Ok(None);
    }
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
    if token.event_type == tev::PATH_ENDED {
        mark_ended(cx.store, &token.run_id)?;
        return Ok(None);
    }
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
    // (T3, codex round 9) A plan gate's `gate.decided` landed: only now does its staged answer
    // apply (the ack recorded first, so a restart finishes it).
    if let Some(staged) = acknowledge_staged(cx.store, &mut session, &pending)? {
        return finish_release(cx, session, staged).map(Some);
    }
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
        // The pause as it stands, to re-open unchanged if the tombstone cannot be written.
        let paused = team.pending.clone().unwrap_or_else(|| pending.clone());
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
            // No publisher to write the tombstone: the actor writes it (there is no publisher
            // draining to contend with). The tombstone is a PRECONDITION of the fallback: if it
            // cannot be written, the answer is not applied — the pause re-opens, still the team
            // handler's (review of #623 round 5).
            match write_tombstone(&run_id, &pending.event_type, reason) {
                Ok(()) => {
                    return on_superseded(cx, &token)
                        .map(|s| s.unwrap_or(SessionStatus::AwaitingHuman));
                }
                Err(why) => {
                    cx.in_flight.remove(&run_id);
                    let session = crate::domain::get_session(cx.store, &run_id)?
                        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
                    return pause_team_transport(cx, session, paused, &format!("{e}; {why}"));
                }
            }
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
    // A terminal run's tombstone (`run_ended`): its end is now on record.
    if crate::domain::get_session(cx.store, &token.run_id)?
        .is_some_and(|s| path_status(s.status).is_some())
    {
        mark_ended(cx.store, &token.run_id)?;
        return Ok(None);
    }
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
    // (T3, codex round 9) The staged plan-gate answer applies now, un-teamed.
    if let Some(staged) = acknowledge_staged(cx.store, &mut session, &pending)? {
        return finish_release(cx, session, staged).map(Some);
    }
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
        TeamBlocked::DisputeApproved => {
            let progress = resume_dispute(&mut cx.act(), session);
            settle(cx, &run_id, progress)
        }
        TeamBlocked::DisputeAmended { note } => {
            let progress = amend_dispute(cx, session, note);
            match progress {
                Ok(s) => Ok(s),
                Err(e) => {
                    cx.in_flight.remove(&run_id);
                    Err(e)
                }
            }
        }
        TeamBlocked::FirstDispatch | TeamBlocked::PlanDispatch { .. } | TeamBlocked::Continue => {
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
/// the store. A run caught while a required fact was publishing re-opens its `team_transport`
/// pause with the fact kept pending and paused (the team handler answers it). A run whose
/// operator had ANSWERED (`Superseding`) has that answer finished, never re-asked: tombstone
/// first, then un-teamed (continue) or returned for cancellation (reject). Runs before the
/// publisher is asked to drain, so no drain can race a tombstone. Returns the runs to cancel.
pub(super) fn reconcile_at_boot(store: &mut dyn GraphStore) -> BootPlan {
    let mut plan = BootPlan::default();
    // No sessions read = nothing reconciled = nothing drained (fail closed): a drain before the
    // reconcile could publish a spooled fact of a run that needed its tombstone first.
    let sessions = match crate::domain::all_sessions(store) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("wicked-core: boot team reconcile could not read sessions ({e}); the team outbox is not drained at boot");
            return plan;
        }
    };
    for mut session in sessions {
        let run_id = session.id.clone();
        if path_status(session.status).is_some() {
            // Terminal: the durable status drives the end, exactly as `run_ended` would have.
            if end_terminal_run(store, &mut session) {
                plan.drain.push(run_id);
            }
            continue;
        }
        let units = match crate::domain::session_units(store, &run_id) {
            Ok(u) => u,
            Err(e) => {
                eprintln!("wicked-core: boot could not read the units of {run_id} ({e}); its team lines are not drained");
                continue;
            }
        };
        if !is_team_run(&session, &units) {
            continue;
        }
        let team = session.team.clone().unwrap_or_default();
        if !team.is_unteamed() && team.stream_floor.is_none() {
            // Tombstone first (whatever the outbox holds for the run), then the store. No
            // tombstone = the run stays undecided and out of the boot drain.
            if let Err(e) = write_tombstone(
                &run_id,
                tev::PATH_STARTED,
                "boot: no acknowledged path.started",
            ) {
                eprintln!("wicked-core: boot could not tombstone team run {run_id} ({e}); leaving it undecided");
                continue;
            }
            let t = session.team.get_or_insert_with(RunTeamState::default);
            set_unteamed(
                t,
                "the daemon restarted before the run's path.started was acknowledged".to_string(),
            );
            if let Err(e) = put_node(store, session.to_node()) {
                // Tombstone-first holds: the next boot finds the same run undecided and repeats.
                eprintln!("wicked-core: boot could not persist un-teamed {run_id}: {e}");
            }
            plan.drain.push(run_id);
            continue;
        }
        if team.is_teamed() && team.pending.is_none() && !has_publisher() {
            // Bus present at launch, absent now: this process cannot publish the run's required
            // facts, so the run is paused `team_transport` (the team handler's gate) — never left
            // live-teamed, never drained, never listed for arming.
            reopen_transport_gate(store, &mut session, &units, unavailable_pending());
            continue;
        }
        let Some(pending) = team.pending.clone() else {
            plan.drain.push(run_id);
            continue;
        };
        match pending.stage {
            PendingStage::Paused => plan.drain.push(run_id),
            // (T3, codex round 9) Acknowledged before the restart, not yet applied: the actor
            // applies it once it is up (it needs the planner's seams), never re-asking.
            PendingStage::Acknowledged => {
                plan.applies.push(run_id.clone());
                plan.drain.push(run_id);
            }
            PendingStage::Superseding => {
                // The operator ANSWERED before the crash (continue without team, or reject); only
                // the publisher's acknowledgement was lost. Finish that answer — never ask again:
                // an open team_transport gate without its paused pending fact would be answered
                // by the generic confirm path (an amendment / a rework, not a transport answer).
                // The tombstone is the durable fact and a PRECONDITION: re-issued first
                // (idempotent), into the state home's outbox whether or not there is a bus.
                let written = write_tombstone(
                    &run_id,
                    &pending.event_type,
                    "boot: finishing a team_transport answer recorded before the restart",
                );
                if let Err(e) = written {
                    // The answer cannot be finished safely: keep the fact pending and PAUSED, so
                    // the team handler (`answer_transport_gate`) takes the next reply. Its lines
                    // stay out of the boot drain.
                    eprintln!(
                        "wicked-core: boot could not tombstone team run {run_id} ({e}); its \
                         team_transport pause re-opens"
                    );
                    reopen_transport_gate(store, &mut session, &units, pending);
                    continue;
                }
                let t = session.team.get_or_insert_with(RunTeamState::default);
                if pending.then == TeamBlocked::Cancel {
                    // Rejected: the run stays teamed so its `path.ended` is published (the one
                    // fact the tombstone lets through); the actor cancels it once it is up.
                    t.pending = None;
                    t.open_gate = None;
                    plan.cancels.push(run_id.clone());
                } else {
                    // Continue without team: un-teamed from here. The run is Executing with no
                    // worker in this process — reported orphaned and resumed like every run a
                    // restart interrupts (`resume_run`), dispatching un-teamed.
                    set_unteamed(
                        t,
                        "the operator chose to continue without team (applied at restart)"
                            .to_string(),
                    );
                    // (T3, codex round 9) A staged plan-gate answer rides the continue: it is
                    // applied (un-teamed) once the actor is up.
                    if pending.staged.is_some() {
                        t.pending = Some(PendingTeamFact {
                            stage: PendingStage::Acknowledged,
                            ..pending.clone()
                        });
                        plan.applies.push(run_id.clone());
                    }
                    session.status = SessionStatus::Executing;
                }
                if let Err(e) = put_node(store, session.to_node()) {
                    eprintln!(
                        "wicked-core: boot could not persist the finished answer of {run_id}: {e}"
                    );
                }
                plan.drain.push(run_id);
            }
            PendingStage::Publishing => {
                // No answer was recorded: a required fact was still publishing. Re-open the
                // pause WITH the fact kept pending and paused, so the team handler answers it.
                reopen_transport_gate(store, &mut session, &units, pending);
                plan.drain.push(run_id);
            }
        }
    }
    plan
}

/// What the boot reconcile leaves for the actor: the rejected runs to cancel once it is up, and
/// the runs whose team lines the boot drain may publish — only runs it READ and reconciled.
#[derive(Debug, Default)]
pub(super) struct BootPlan {
    pub cancels: Vec<String>,
    /// Runs whose acknowledged, staged plan-gate answer the actor applies once it is up.
    pub applies: Vec<String>,
    pub drain: Vec<String>,
}

/// Re-open a run's `team_transport` pause at boot over `pending`, kept PAUSED so
/// `transport_gate_open` holds and `answer_transport_gate` — never the generic confirm path —
/// takes the reply.
fn reopen_transport_gate(
    store: &mut dyn GraphStore,
    session: &mut AgentSession,
    units: &[WorkUnit],
    pending: PendingTeamFact,
) {
    let run_id = session.id.clone();
    let t = session.team.get_or_insert_with(RunTeamState::default);
    t.gate_seq += 1;
    t.open_gate = Some(tev::gate_id(&run_id, t.gate_seq));
    t.pending = Some(PendingTeamFact {
        stage: PendingStage::Paused,
        ..pending.clone()
    });
    let ord = units.get(session.unit_ix).map(|u| u.ord).unwrap_or(0);
    session.status = SessionStatus::AwaitingHuman;
    let prompt = if pending.event_type == TRANSPORT_UNAVAILABLE {
        transport_prompt(
            TRANSPORT_UNAVAILABLE,
            "the daemon restarted with no team publisher (no bus) for this teamed run",
        )
    } else {
        format!(
            "Team transport: the daemon restarted while `{}` was in flight. Approve to retry, \
             approve with amend \"{CONTINUE_WITHOUT_TEAM}\" to run un-teamed, or reject to cancel.",
            pending.event_type
        )
    };
    let request = crate::interaction::open_gate(
        &run_id,
        ord,
        None,
        &prompt,
        TEAM_TRANSPORT_GATE,
        crate::interaction::now_millis(),
    );
    if let Err(e) = crate::domain::put_nodes(store, &[session.to_node(), request.to_node()]) {
        // The run keeps its previous durable state; the next boot re-opens the pause again.
        eprintln!("wicked-core: boot could not re-open the team_transport pause of {run_id}: {e}");
    }
}

/// Tell the publisher to drain the outbox once, for the runs the boot reconcile reconciled. No
/// publisher (no bus) = nothing to drain onto; the lines stay for the bus's return.
pub(super) fn drain_at_boot(runs: Vec<String>) {
    let _ = send(PublisherReq::DrainRuns(runs));
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
                PendingStage::Publishing | PendingStage::Superseding | PendingStage::Acknowledged
            )
        })
}

/// An acknowledgement handler that FAILED answers its run's held reply with the error (the one
/// outcome the durable status cannot carry). Every other outcome is settled by
/// [`settle_held_replies`] from the run's status.
pub(super) fn fail_held_reply(
    replies: &mut HashMap<String, Sender<anyhow::Result<SessionStatus>>>,
    run_id: &str,
    res: anyhow::Result<Option<SessionStatus>>,
) {
    if let Err(e) = res {
        eprintln!("wicked-core: team acknowledgement for {run_id} failed: {e:#}");
        if let Some(reply) = replies.remove(run_id) {
            let _ = reply.send(Err(e));
        }
    }
}

/// Settle every held `confirm_gate` reply from its run's DURABLE status (review of #623 round
/// 4): a run that ended — cancelled, failed, completed, by whatever path — or whose answer is no
/// longer waiting on the publisher is answered with its current status; an unknown run with an
/// error. Called by the actor after every command, so no held sender outlives its run.
pub(super) fn settle_held_replies(
    store: &dyn GraphStore,
    replies: &mut HashMap<String, Sender<anyhow::Result<SessionStatus>>>,
) {
    replies.retain(|run_id, reply| {
        let session = match crate::domain::get_session(store, run_id) {
            Ok(Some(s)) => s,
            Ok(None) => {
                let _ = reply.send(Err(anyhow::anyhow!("run not found: {run_id}")));
                return false;
            }
            // A store read fault: keep waiting; the next command retries.
            Err(_) => return true,
        };
        let waiting = path_status(session.status).is_none()
            && session
                .team
                .as_ref()
                .and_then(|t| t.pending.as_ref())
                .is_some_and(|p| {
                    matches!(
                        p.stage,
                        PendingStage::Publishing
                            | PendingStage::Superseding
                            | PendingStage::Acknowledged
                    )
                });
        if waiting {
            return true;
        }
        let _ = reply.send(Ok(session.status));
        false
    });
}

// ── DES-TEAMING-002 T6: the team_dispute gate and member steps (§8.8, §8.10; DES-001 §6.7) ──────

/// A member step is rejected at most this many times before the next rejection goes to the PA
/// (DES-002 §8.8 `MAX_STEP_REWORK`).
pub(super) const MAX_STEP_REWORK: u32 = 2;

/// The actor handles a team step needs, from `apply_step_result` or an acknowledgement handler.
pub(super) struct Act<'a> {
    pub store: &'a mut dyn GraphStore,
    pub subscribers: &'a mut crate::event_log::EventSink,
    pub runner: &'a Arc<dyn StepRunner>,
    pub self_tx: &'a Sender<Command>,
    pub lifecycle_maps: &'a Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    pub actor_maps: &'a Option<Arc<std::sync::Mutex<ElicitationMaps>>>,
    pub process_gen: uuid::Uuid,
    pub is_acp: bool,
}

impl Ctx<'_> {
    fn act(&mut self) -> Act<'_> {
        Act {
            store: &mut *self.store,
            subscribers: &mut *self.subscribers,
            runner: self.runner,
            self_tx: self.self_tx,
            lifecycle_maps: self.lifecycle_maps,
            actor_maps: self.actor_maps,
            process_gen: self.process_gen,
            is_acp: self.is_acp,
        }
    }
}

/// Map a team step's progress onto the run's status and its in-flight marker.
fn settle(
    cx: &mut Ctx<'_>,
    run_id: &str,
    progress: anyhow::Result<Progress>,
) -> anyhow::Result<SessionStatus> {
    match progress {
        Ok(Progress::Dispatched) | Ok(Progress::Deferred) => {
            cx.in_flight.insert(run_id.to_string());
            Ok(SessionStatus::Executing)
        }
        Ok(Progress::Paused) => {
            cx.in_flight.remove(run_id);
            Ok(SessionStatus::AwaitingHuman)
        }
        Ok(Progress::Done) => {
            cx.in_flight.remove(run_id);
            finalize_run(cx.store, cx.subscribers, cx.runner, cx.self_tx, run_id)?;
            Ok(SessionStatus::Completed)
        }
        Err(e) => {
            cx.in_flight.remove(run_id);
            fail_run_by_id(
                cx.store,
                cx.subscribers,
                cx.runner,
                cx.self_tx,
                run_id,
                anyhow::anyhow!("{e:#}"),
            );
            Err(e)
        }
    }
}

/// Whether `session` is paused on a `team_dispute` gate (its answer is the team handler's).
pub(super) fn dispute_gate_open(session: &AgentSession) -> bool {
    session.status == SessionStatus::AwaitingHuman
        && session.team.as_ref().is_some_and(|t| t.dispute.is_some())
}

/// Open a `team_dispute` pause over unit `ord` (DES-001 §6.7): mint the gate, publish
/// `gate.opened{kind:"team_dispute"}` (its pause is durable in the store either way, P1 row 3),
/// record the gate on the run, and pause. The run's cursor stays on the unit.
pub(super) fn open_dispute(
    act: &mut Act<'_>,
    session: &mut AgentSession,
    ord: u32,
    attempt: u32,
    kind: crate::domain::DisputeKind,
    prompt: String,
    finding_ids: Vec<String>,
) -> anyhow::Result<Progress> {
    let run_id = session.id.clone();
    let team = session.team.get_or_insert_with(Default::default);
    team.gate_seq += 1;
    let gid = tev::gate_id(&run_id, team.gate_seq);
    team.dispute = Some(crate::domain::DisputeGate {
        gate_id: gid.clone(),
        ord,
        attempt,
        kind,
        finding_ids: finding_ids.clone(),
    });
    publish_fire(unit_event(
        &run_id,
        ord,
        attempt,
        TeamBody::GateOpened(GateOpened {
            gate_id: gid,
            kind: GateOpenedKind::TeamDispute { finding_ids },
        }),
    ));
    pause_for_human(
        act.store,
        act.subscribers,
        act.self_tx,
        session,
        ord,
        None,
        TEAM_DISPUTE_GATE,
        prompt,
    )?;
    Ok(Progress::Paused)
}

/// Answer a `team_dispute` pause (the row is already resolved). Every answer first publishes the
/// gate's `gate.decided{by:"human"}` — a REQUIRED fact (§4.1): the run resumes, reworks or
/// cancels only on its acknowledgement; past the bound it stays paused `team_transport`.
pub(super) fn answer_dispute_gate(
    cx: &mut Ctx<'_>,
    mut session: AgentSession,
    decision: crate::workflow::HumanDecision,
) -> anyhow::Result<SessionStatus> {
    let run_id = session.id.clone();
    let Some(d) = session.team.as_ref().and_then(|t| t.dispute.clone()) else {
        anyhow::bail!("run {run_id} is not paused team_dispute");
    };
    let (decision_token, then) = match decision {
        crate::workflow::HumanDecision::Reject => {
            (GateDecision::HumanRejected, TeamBlocked::Cancel)
        }
        crate::workflow::HumanDecision::RequestChanges { note } => (
            GateDecision::HumanAmended,
            TeamBlocked::DisputeAmended {
                note: note.unwrap_or_default(),
            },
        ),
        crate::workflow::HumanDecision::Approve { amend: Some(a), .. } if !a.trim().is_empty() => (
            GateDecision::HumanAmended,
            TeamBlocked::DisputeAmended { note: a },
        ),
        crate::workflow::HumanDecision::Approve { .. } => {
            (GateDecision::HumanApproved, TeamBlocked::DisputeApproved)
        }
        crate::workflow::HumanDecision::EditPlan { .. } => {
            anyhow::bail!("run {run_id} is paused team_dispute: a plan edit answers only a plan_approval gate")
        }
    };
    let ev = TeamEvent {
        env: Envelope {
            ord: Some(d.ord),
            attempt: Some(d.attempt),
            by: "human".to_string(),
            re: Some(format!("gate.opened#{}", d.gate_id)),
            ..envelope(&run_id)
        },
        body: TeamBody::GateDecided(GateDecided {
            gate_id: d.gate_id.clone(),
            kind: GateKind::TeamDispute,
            decision: decision_token,
            combined: None,
            team_pause: d.kind == crate::domain::DisputeKind::Finding,
            unresolved: d.finding_ids.clone(),
        }),
    };
    let key = ev.key()?;
    match publish_required(ev, Exhausted::Keep) {
        Ok(token) => {
            let team = session.team.get_or_insert_with(RunTeamState::default);
            team.open_gate = Some(d.gate_id.clone());
            team.pending = Some(PendingTeamFact {
                event_type: token.event_type,
                key: token.key,
                stage: PendingStage::Publishing,
                then,
                staged: None,
            });
            put_node(cx.store, session.to_node())?;
            cx.in_flight.insert(run_id);
            Ok(SessionStatus::AwaitingHuman)
        }
        Err(reason) => {
            // No publisher to take the required fact: the run stays paused, now `team_transport`
            // over the decision (its answer carries the step on; nothing resumes unacknowledged).
            let pending = PendingTeamFact {
                event_type: tev::GATE_DECIDED.to_string(),
                key,
                stage: PendingStage::Paused,
                then,
                staged: None,
            };
            pause_team_transport(cx, session, pending, &reason)
        }
    }
}

/// `DisputeApproved`, once the human's `gate.decided` landed: `resumed`, then — for a finding
/// dispute — the `gateDecided{allow:true}` and `unitDone` the fold withheld (DES-001 #13, #16 g),
/// and the run advances past the unit. Never a re-dispatch, never an attempt bump. A member's step
/// counts (a human approved it), or — a finding dispute on a member's work — goes to the PA's
/// review.
fn resume_dispute(act: &mut Act<'_>, mut session: AgentSession) -> anyhow::Result<Progress> {
    let run_id = session.id.clone();
    let Some(d) = session.team.as_mut().and_then(|t| t.dispute.take()) else {
        anyhow::bail!("run {run_id}: no team_dispute gate on record to resume");
    };
    let units = crate::domain::session_units(act.store, &run_id)?;
    let ix = units
        .iter()
        .position(|u| u.ord == d.ord)
        .ok_or_else(|| anyhow::anyhow!("run {run_id}: no unit {} to resume", d.ord))?;
    session.status = SessionStatus::Executing;
    let counts_now = d.kind == crate::domain::DisputeKind::Finding && !is_member_work(&units[ix]);
    if counts_now {
        // The unit counts: the cursor leaves it in the SAME write that clears the dispute, so a
        // restart never finds a done unit with a pausing ledger at the cursor and no gate open.
        session.unit_ix = ix + 1;
        session.attempt = units.get(ix + 1).map(next_attempt).unwrap_or(0);
    }
    put_node(act.store, session.to_node())?;
    emit(
        act.subscribers,
        CoreEvent::Resumed {
            session: run_id.clone(),
            ord: d.ord,
        },
    );
    match d.kind {
        crate::domain::DisputeKind::Finding => {
            if !counts_now {
                return enter_review(act, &run_id, ix, d.attempt);
            }
            emit_counted(act, &run_id, d.ord);
            advance_or_pause(
                act.store,
                act.subscribers,
                act.runner,
                act.self_tx,
                &run_id,
                ix + 1,
                act.lifecycle_maps,
                act.actor_maps,
                act.process_gen,
                act.is_acp,
            )
        }
        crate::domain::DisputeKind::MemberStep => {
            accept_member_step(act, &run_id, ix, Some("a human approved it".into()))
        }
    }
}

/// `DisputeAmended`, once the human's `gate.decided` landed: a finding dispute reruns the creator
/// with the note (`rewind_to_creator`, DES-001 #16 k); a member-step dispute goes back to the
/// member with the note as its rework amendment.
fn amend_dispute(
    cx: &mut Ctx<'_>,
    mut session: AgentSession,
    note: String,
) -> anyhow::Result<SessionStatus> {
    let run_id = session.id.clone();
    let Some(d) = session.team.as_mut().and_then(|t| t.dispute.take()) else {
        anyhow::bail!("run {run_id}: no team_dispute gate on record to amend");
    };
    put_node(cx.store, session.to_node())?;
    match d.kind {
        crate::domain::DisputeKind::Finding => {
            let reopen = session.clone();
            match rewind_to_creator(
                cx.store,
                cx.subscribers,
                cx.runner,
                cx.self_tx,
                cx.in_flight,
                session,
                &run_id,
                Some(note),
                cx.lifecycle_maps,
                cx.actor_maps,
                cx.process_gen,
                cx.is_acp,
            ) {
                Ok(s) => Ok(s),
                // The rework could not start (refused before the answer resolved in the normal
                // case): the dispute re-opens, still answerable — never a run paused on nothing.
                Err(e) => {
                    let mut reopen = reopen;
                    open_dispute(
                        &mut cx.act(),
                        &mut reopen,
                        d.ord,
                        d.attempt,
                        d.kind,
                        format!("Team dispute on unit {}: the rework could not start ({e:#}). Approve to count the work, or reject to cancel.", d.ord),
                        d.finding_ids.clone(),
                    )?;
                    cx.in_flight.remove(&run_id);
                    Ok(SessionStatus::AwaitingHuman)
                }
            }
        }
        crate::domain::DisputeKind::MemberStep => {
            let units = crate::domain::session_units(cx.store, &run_id)?;
            let ix = units
                .iter()
                .position(|u| u.ord == d.ord)
                .ok_or_else(|| anyhow::anyhow!("run {run_id}: no unit {}", d.ord))?;
            let progress = rework_member_step(
                &mut cx.act(),
                &run_id,
                ix,
                crate::team::events::ReworkBy::Member,
                format!("operator: {note}"),
                false,
            );
            settle(cx, &run_id, progress)
        }
    }
}

/// The `gateDecided{allow:true}` + `unitDone` a withheld fold owes, now that the unit counts.
fn emit_counted(act: &mut Act<'_>, run_id: &str, ord: u32) {
    emit(
        act.subscribers,
        CoreEvent::GateDecided {
            session: run_id.to_string(),
            ord,
            allow: true,
        },
    );
    emit(
        act.subscribers,
        CoreEvent::UnitDone {
            session: run_id.to_string(),
            ord,
        },
    );
}

/// Move the cursor past unit `ix` and advance (dispatch, pause at the next gate, or finish).
pub(super) fn advance_past(act: &mut Act<'_>, run_id: &str, ix: usize) -> anyhow::Result<Progress> {
    let mut session = crate::domain::get_session(act.store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let units = crate::domain::session_units(act.store, run_id)?;
    session.unit_ix = ix + 1;
    session.attempt = units.get(ix + 1).map(next_attempt).unwrap_or(0);
    session.status = SessionStatus::Executing;
    put_node(act.store, session.to_node())?;
    advance_or_pause(
        act.store,
        act.subscribers,
        act.runner,
        act.self_tx,
        run_id,
        ix + 1,
        act.lifecycle_maps,
        act.actor_maps,
        act.process_gen,
        act.is_acp,
    )
}

/// The PA seat instance (`path.started.cli`): the first of the run's seats.
fn pa_seat(session: &AgentSession) -> String {
    session
        .clis
        .first()
        .cloned()
        .unwrap_or_else(|| "claude".to_string())
}

/// Whether `u` is a member's work that has not been reviewed yet (its next step is the PA's
/// review, not the gate's count).
pub(super) fn is_member_work(u: &WorkUnit) -> bool {
    u.team_run
        && u.owner == crate::workflow::StepOwner::Team
        && u.member_step
            .as_ref()
            .is_some_and(|m| !m.replanned && !m.counted && m.reviewing.is_none())
}

/// The team step a DONE cursor unit still owes (a restart between the fold's unit write and the
/// step that follows it): the PA's review of a member's work, or the `team_dispute` pause its
/// ledger calls for. Never skipped as "done": a restart can neither count a member's step nor
/// continue past an unresolved HIGH. `None` for every other unit.
pub(super) fn owed_team_step(session: &AgentSession, u: &WorkUnit) -> Option<OwedStep> {
    if u.status != crate::domain::UnitStatus::Done || !u.team_run {
        return None;
    }
    let ledger_pauses = u
        .team
        .as_ref()
        .filter(|t| t.transport == Transport::Bus)
        .map(|t| t.ledger.as_ref().is_none_or(|l| l.team_pause));
    let open = session.team.as_ref().is_some_and(|t| t.dispute.is_some());
    let counted = u.member_step.as_ref().is_some_and(|m| m.counted);
    if ledger_pauses == Some(true) && !open && !counted {
        return Some(OwedStep::Dispute);
    }
    is_member_work(u).then_some(OwedStep::Review)
}

/// See [`owed_team_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum OwedStep {
    Review,
    Dispute,
}

/// Perform the team step a done cursor unit owes ([`owed_team_step`]).
pub(super) fn run_owed_step(
    act: &mut Act<'_>,
    mut session: AgentSession,
    ix: usize,
    step: OwedStep,
) -> anyhow::Result<Progress> {
    let run_id = session.id.clone();
    let units = crate::domain::session_units(act.store, &run_id)?;
    let unit = units[ix].clone();
    let attempt = unit.last_attempt.unwrap_or(session.attempt);
    match step {
        OwedStep::Review => enter_review(act, &run_id, ix, attempt),
        OwedStep::Dispute => {
            let finding_ids: Vec<String> = unit
                .team
                .as_ref()
                .and_then(|t| t.ledger.as_ref())
                .map(|l| {
                    tev::unresolved_highs(l)
                        .iter()
                        .map(|f| f.finding.finding_id.clone())
                        .collect()
                })
                .unwrap_or_default();
            open_dispute(
                act,
                &mut session,
                unit.ord,
                attempt,
                crate::domain::DisputeKind::Finding,
                format!(
                    "Team dispute on unit {} ({}): the gate approved the work, but its team \
                     record does not let the run continue unattended (restored after a restart). \
                     Approve to continue, request changes to rework, or reject to cancel.",
                    unit.ord, unit.description
                ),
                finding_ids,
            )
        }
    }
}

/// Whether `u`'s current attempt is the PA's review of a member's step.
pub(super) fn is_review(u: &WorkUnit) -> bool {
    u.member_step
        .as_ref()
        .is_some_and(|m| m.reviewing.is_some())
}

/// A team unit's member step state at dispatch (DES-002 §8.8): an `owner: team` step of a team
/// run belongs to the member seat distribution put it on. A member step that landed on the PA's
/// own seat is not a member's step: it is the PA's, gated as any other (distribution refuses a
/// team run with no seat distinct from the PA, so this is a disclosed corner, never a bypass).
pub(super) fn stamp_member_step(session: &AgentSession, unit: &mut WorkUnit) -> bool {
    if !unit.team_run
        || unit.owner != crate::workflow::StepOwner::Team
        || unit.tool_cmd.is_some()
        || unit.member_step.is_some()
    {
        return false;
    }
    let member = unit
        .assigned_cli
        .clone()
        .unwrap_or_else(|| "claude".to_string());
    let replanned = member == pa_seat(session);
    if replanned {
        eprintln!(
            "wicked-core: run {} unit {}: the member step landed on the PA's seat {member}; it runs \
             as the PA's own step",
            session.id, unit.ord
        );
    }
    unit.member_step = Some(crate::domain::MemberStepState {
        member,
        replanned,
        ..Default::default()
    });
    true
}

/// The member's work passed its gate: hold the count and dispatch the PA's review of it on the PA
/// seat (the unit's next attempt). The member attempt's snapshot is kept for the count.
pub(super) fn enter_review(
    act: &mut Act<'_>,
    run_id: &str,
    ix: usize,
    member_attempt: u32,
) -> anyhow::Result<Progress> {
    let mut session = crate::domain::get_session(act.store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let mut units = crate::domain::session_units(act.store, run_id)?;
    let unit = &mut units[ix];
    let pa = pa_seat(&session);
    let work_team = unit.team.clone();
    let ms = unit.member_step.get_or_insert_with(Default::default);
    ms.reviewing = Some(member_attempt);
    ms.work_team = work_team;
    unit.assigned_cli = Some(pa);
    unit.status = crate::domain::UnitStatus::Pending;
    put_node(act.store, unit.to_node())?;
    session.unit_ix = ix;
    session.attempt = next_attempt(unit);
    session.status = SessionStatus::Executing;
    put_node(act.store, session.to_node())?;
    dispatch_unit(
        act.store,
        act.subscribers,
        act.runner,
        act.self_tx,
        run_id,
        ix,
        act.lifecycle_maps,
        act.actor_maps,
        act.process_gen,
        act.is_acp,
    )?;
    Ok(Progress::Dispatched)
}

/// The member's step counts (the PA accepted it, a council said YES, or a human approved it): the
/// unit is done with the member's output as its work output, its member attempt's snapshot is its
/// evidence again, the withheld `gateDecided` + `unitDone` are emitted, and the run advances.
fn accept_member_step(
    act: &mut Act<'_>,
    run_id: &str,
    ix: usize,
    dissent: Option<String>,
) -> anyhow::Result<Progress> {
    let mut units = crate::domain::session_units(act.store, run_id)?;
    let unit = &mut units[ix];
    let ord = unit.ord;
    if let Some(ms) = unit.member_step.as_mut() {
        ms.reviewing = None;
        ms.counted = true;
        if let Some(t) = ms.work_team.take() {
            unit.team = Some(t);
        }
        unit.assigned_cli = Some(ms.member.clone());
        if let (Some(why), Some(last)) = (dissent, ms.reviews.last_mut()) {
            last.member_reason.get_or_insert(why);
        }
    }
    unit.status = crate::domain::UnitStatus::Done;
    put_node(act.store, unit.to_node())?;
    emit_counted(act, run_id, ord);
    advance_past(act, run_id, ix)
}

/// The PA's rejection stands: rework the step — back to the member with the reason as its
/// amendment, or re-planned onto the PA (`to:pa`, or past `MAX_STEP_REWORK` rejections).
fn rework_member_step(
    act: &mut Act<'_>,
    run_id: &str,
    ix: usize,
    to: crate::team::events::ReworkBy,
    reason: String,
    count: bool,
) -> anyhow::Result<Progress> {
    let mut session = crate::domain::get_session(act.store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let mut units = crate::domain::session_units(act.store, run_id)?;
    let pa = pa_seat(&session);
    let unit = &mut units[ix];
    let ord = unit.ord;
    let ms = unit.member_step.get_or_insert_with(Default::default);
    if count {
        ms.rejections += 1;
    }
    ms.reviewing = None;
    ms.work_team = None;
    let to_pa = to == crate::team::events::ReworkBy::Pa || ms.rejections > MAX_STEP_REWORK;
    if to_pa {
        ms.replanned = true;
        unit.owner = crate::workflow::StepOwner::Pa;
        unit.assigned_cli = Some(pa);
    } else {
        unit.assigned_cli = Some(ms.member.clone());
    }
    let amendment = format!(
        "{} — {reason}",
        if to_pa {
            "The PA rejected the member's output; the step is re-planned onto the PA"
        } else {
            "The PA rejected your output for this step; rework it"
        }
    );
    unit.status = crate::domain::UnitStatus::Distributed;
    unit.rework_of = Some(ord);
    unit.rework_amendment = Some(amendment.clone());
    unit.worktree_baseline = None;
    unit.worktree_mutation = None;
    unit.denial = None;
    unit.denial_reason = None;
    put_node(act.store, unit.to_node())?;
    session.unit_ix = ix;
    session.attempt = next_attempt(unit);
    session.status = SessionStatus::Executing;
    put_node(act.store, session.to_node())?;
    emit(
        act.subscribers,
        CoreEvent::UnitReworkAmended {
            session: run_id.to_string(),
            ord,
            amendment,
            updated_description: unit.description.clone(),
            scope: "member_step".to_string(),
        },
    );
    dispatch_unit(
        act.store,
        act.subscribers,
        act.runner,
        act.self_tx,
        run_id,
        ix,
        act.lifecycle_maps,
        act.actor_maps,
        act.process_gen,
        act.is_acp,
    )?;
    Ok(Progress::Dispatched)
}

/// Apply the PA's review attempt of a member's step (DES-002 §8.8). The engine reads the `STEP`
/// verdict from the output it was handed (the same line R published as `step.reviewed`) and the
/// member's answer and any council ruling from the attempt's ledger (S's fold):
///
/// - ACCEPT → the step counts;
/// - REJECT, the member held, council YES → the step counts (the rejection is dissent);
/// - REJECT, the member took the rejection, or the council said NO → the rejection stands;
/// - anything not on record — no `STEP` line, a failed review turn, an incomplete team record, no
///   member answer to a rejection, a council with no verdict → a `team_dispute` pause. Nothing
///   absent ever counts the step.
pub(super) fn apply_review(
    act: &mut Act<'_>,
    mut session: AgentSession,
    ix: usize,
    output: &crate::workflow::StepOutput,
    worker: Option<UnitTeamSnapshot>,
) -> anyhow::Result<Progress> {
    let run_id = session.id.clone();
    let mut units = crate::domain::session_units(act.store, &run_id)?;
    let unit = units[ix].clone();
    let ms = unit.member_step.clone().unwrap_or_default();
    let reviewed = ms.reviewing.unwrap_or_default();
    let step_id = unit
        .phase_id()
        .map(str::to_string)
        .unwrap_or_else(|| format!("unit-{}", unit.ord));
    let snap = crate::team::runner::merge_snapshot(unit.team.as_ref(), worker);
    let ledger = snap.as_ref().and_then(|s| s.ledger.clone());
    let line = (output.status == crate::workflow::StepStatus::Ok)
        .then(|| {
            crate::team::parse_step_lines(&output.output)
                .into_iter()
                .find(|l| l.step_id == step_id)
        })
        .flatten();
    let record = ledger.as_ref().and_then(|l| {
        l.step_reviews
            .iter()
            .find(|r| r.step_id == step_id)
            .cloned()
    });
    // The durable record of this review, whatever it comes to.
    if let Some(l) = &line {
        let mut rec = record.clone().unwrap_or(crate::team::StepReviewRecord {
            step_id: step_id.clone(),
            reviewed_attempt: reviewed,
            verdict: l.verdict,
            to: l.to,
            reason: l.reason.clone(),
            held: None,
            member_reason: None,
            dispute: None,
        });
        rec.verdict = l.verdict;
        rec.to = l.to;
        rec.reason = l.reason.clone();
        let u = &mut units[ix];
        u.member_step
            .get_or_insert_with(Default::default)
            .reviews
            .push(rec);
        put_node(act.store, u.to_node())?;
    }
    let pause = |act: &mut Act<'_>, session: &mut AgentSession, why: String| {
        open_dispute(
            act,
            session,
            unit.ord,
            output.attempt,
            crate::domain::DisputeKind::MemberStep,
            format!(
                "Team dispute on member step `{step_id}` (unit {}, by {}): {why}. Approve to \
                 count the member's output, request changes to send it back to the member, or \
                 reject to cancel.",
                unit.ord, ms.member
            ),
            Vec::new(),
        )
    };
    let Some(line) = line else {
        return pause(
            act,
            &mut session,
            format!(
                "the PA's review turn gave no `STEP {step_id}: ACCEPT|REJECT` verdict (status {:?})",
                output.status
            ),
        );
    };
    if ledger.as_ref().is_none_or(|l| l.team_pause) {
        return pause(
            act,
            &mut session,
            "the team's record of the review is incomplete".to_string(),
        );
    }
    if line.verdict == crate::team::events::StepVerdict::Accepted {
        return accept_member_step(act, &run_id, ix, None);
    }
    let held = record.as_ref().and_then(|r| r.held);
    let verdict = record
        .as_ref()
        .and_then(|r| r.dispute.as_ref())
        .map(|d| d.verdict);
    match (held, verdict) {
        (Some(true), Some(crate::team::Verdict::Yes)) => accept_member_step(
            act,
            &run_id,
            ix,
            Some(format!(
                "council YES over the PA's rejection: {}",
                line.reason
            )),
        ),
        (Some(true), Some(crate::team::Verdict::No)) | (Some(false), _) => rework_member_step(
            act,
            &run_id,
            ix,
            line.to.unwrap_or(crate::team::events::ReworkBy::Member),
            line.reason.clone(),
            true,
        ),
        (Some(true), _) => pause(
            act,
            &mut session,
            "the member held its output and the council gave no verdict".to_string(),
        ),
        (None, _) => pause(
            act,
            &mut session,
            "the PA rejected it and the member's answer is not on record".to_string(),
        ),
    }
}

// ── T4: re-plan (DES-TEAMING-002 §8.7) ───────────────────────────────────────────────────────────

/// The run's repo root, for the graph a score reads.
fn repo_root(store: &dyn GraphStore, session: &AgentSession) -> Option<PathBuf> {
    session
        .repo_ref
        .as_deref()
        .and_then(|id| crate::repo::get_repo(store, id).ok().flatten())
        .map(|r| PathBuf::from(r.root_path))
}

/// (T4) The supervisor's diff measurement (`Command::TeamRescored`): score it against the run's
/// graph; when its band rises above the run's ratcheted floor (§8.5), hold it for the next step
/// boundary (the highest one waiting wins). A lower score changes and publishes nothing.
pub(super) fn on_rescored(
    store: &mut dyn GraphStore,
    run_id: &str,
    ord: u32,
    attempt: u32,
    rescore_seq: u32,
    tree: &str,
    paths: &[String],
) -> anyhow::Result<()> {
    let Some(mut session) = crate::domain::get_session(&*store, run_id)? else {
        return Ok(());
    };
    if !matches!(
        session.status,
        SessionStatus::Executing | SessionStatus::AwaitingHuman
    ) || paths.is_empty()
    {
        return Ok(());
    }
    let root = repo_root(&*store, &session);
    let Some(tp) = session.team_plan.as_ref().filter(|t| t.accepted_rev > 0) else {
        return Ok(());
    };
    let scored = crate::plan_gate::diff_score_for_run(
        paths,
        root.as_deref(),
        session.base_commit.as_deref(),
    );
    let score = scored.assessment.score;
    if !crate::plan_gate::floor_rises(tp, score, scored.destructive) {
        return Ok(());
    }
    if let Some(r) = &tp.rescored {
        if r.score >= score && (r.destructive || !scored.destructive) {
            return Ok(());
        }
    }
    let fact = crate::plan_gate::path_scored_diff(
        run_id,
        ord,
        attempt,
        rescore_seq,
        Some(tree),
        &scored.assessment,
        crate::interaction::now_millis(),
    )?;
    let rescored = crate::plan_gate::DiffRescore {
        ord,
        attempt,
        rescore_seq,
        score,
        destructive: scored.destructive,
        fact: crate::plan_gate::queued_facts(&[fact])?
            .pop()
            .ok_or_else(|| anyhow::anyhow!("no path.scored fact"))?,
    };
    if let Some(tp) = session.team_plan.as_mut() {
        tp.rescored = Some(rescored);
    }
    put_node(store, session.to_node())
}

/// (T4) The step boundary (§8.7 "Applying a revision"): after the finished unit folds and before
/// the next dispatch. The triggers, in order: a held diff re-score that raised the floor, then the
/// PA's `PLAN <change_id>: ACCEPT` / `PLAN+` lines of this output. Each is one revision (`rev`
/// n+1, n+2, …); the new units are inserted after the cursor in one pass, the done prefix is never
/// touched, and a revision the approval matrix holds leaves the plan pending, so the run pauses
/// `plan_approval` before its next unit (the caller's `advance_or_pause`).
pub(super) fn revise_at_boundary(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    run_id: &str,
    output: &crate::workflow::StepOutput,
) -> anyhow::Result<()> {
    let Some(mut session) = crate::domain::get_session(&*store, run_id)? else {
        return Ok(());
    };
    let Some(prior) = session
        .team_plan
        .clone()
        .filter(|t| t.accepted_rev > 0 && t.pending.is_none())
    else {
        return Ok(());
    };
    let units = crate::domain::session_units(&*store, run_id)?;
    let cursor = session.unit_ix.min(units.len());
    let done: Vec<String> = units[..cursor]
        .iter()
        .map(|u| u.phase_id().unwrap_or_default().to_string())
        .collect();
    let finished = units.get(output.unit_ix);
    let mut changes = Vec::new();
    if let Some(r) = prior.rescored.clone() {
        changes.push(crate::plan_gate::Change::Floor(r));
    }
    // Only the PA's own turn speaks for the plan (§8.8: the PA owns a member's step): its step,
    // or its review of a member's step.
    let pa = session.clis.first().cloned().unwrap_or_default();
    if let Some(u) = finished.filter(|_| output.status == crate::workflow::StepStatus::Ok) {
        let reviewing = u
            .member_step
            .as_ref()
            .is_some_and(|m| m.reviewing.is_some());
        if u.assigned_cli.as_deref() == Some(pa.as_str()) || reviewing {
            changes.extend(crate::plan_gate::changes_from_output(
                &output.output,
                &pa,
                u.ord,
                output.attempt,
            ));
        }
    }
    if changes.is_empty() {
        return Ok(());
    }
    if done.iter().any(|d| d == "deliver") {
        // Nothing runs after the push: a revision there would ship unreviewed. Say so; the plan
        // stays as it is.
        anyhow::bail!("the plan was not revised: its deliver step already ran");
    }
    let mut state = prior.clone();
    state.rescored = None;
    let mut facts = Vec::new();
    let mut def = None;
    let now = crate::interaction::now_millis();
    let reviewing_ord = finished.map(|u| u.ord);
    for change in changes {
        let r = match crate::plan_gate::revise(
            run_id,
            &state,
            change,
            &done,
            &session.human_confirm,
            reviewing_ord,
            now,
        ) {
            Ok(r) => r,
            Err(e) => {
                emit_run_error(subscribers, run_id, e);
                continue;
            }
        };
        facts.extend(r.events);
        match r.outcome {
            crate::plan_gate::Outcome::Accepted { def: d }
            | crate::plan_gate::Outcome::Held { def: d } => {
                state = r.state;
                def = Some(d);
            }
            crate::plan_gate::Outcome::Refused { .. } => {}
        }
    }
    session.team_plan = Some(state);
    put_node(store, session.to_node())?;
    if let Some(def) = def {
        revise_units(store, subscribers, &session, def)?;
    }
    publish_plan_facts(&session, facts);
    Ok(())
}

/// (T4, §8.7 "Mechanism") Insert a revision's units into the live run: plan the def (every
/// synchronous planning check, the pins attached), distribute it on the launch roster (the whole
/// plan, so evaluator ≠ creator holds against the done units too), and write ONLY the units after
/// the cursor — renumbered from it — leaving every unit already dispatched or done untouched.
/// The cursor unit's attempt is re-read: a new unit there is dispatched at attempt 0.
pub(super) fn revise_units(
    store: &mut dyn GraphStore,
    subscribers: &mut crate::event_log::EventSink,
    session: &AgentSession,
    def: crate::workflow::WorkflowDef,
) -> anyhow::Result<()> {
    let run_id = session.id.as_str();
    let old = crate::domain::session_units(&*store, run_id)?;
    let cursor = session.unit_ix.min(old.len());
    let planned = super::check_def_plans(
        store,
        &def,
        &session.problem,
        run_id,
        session.repo_ref.as_deref(),
    )?;
    for (o, n) in old[..cursor].iter().zip(&planned) {
        if o.id != n.id || o.ord != n.ord {
            anyhow::bail!(
                "run {run_id}: the revised plan moves the unit `{}` that already ran",
                o.id
            );
        }
    }
    let roster = super::launch_roster(session)?;
    let dists = crate::distribute::distribute_units_on_benched(
        &planned,
        &roster,
        run_id,
        &session.benched_seats,
    )?;
    let old_ids: std::collections::HashSet<&str> = old.iter().map(|u| u.id.as_str()).collect();
    let tail: Vec<WorkUnit> = planned.iter().skip(cursor).cloned().collect();
    for u in tail.iter().filter(|u| !old_ids.contains(u.id.as_str())) {
        emit(
            subscribers,
            CoreEvent::UnitPlanned {
                session: run_id.to_string(),
                ord: u.ord,
                description: u.description.clone(),
                stage: u.stage.label().to_string(),
                role: match u.role {
                    crate::workflow::PhaseRole::Neutral => "neutral",
                    crate::workflow::PhaseRole::Creator => "creator",
                    crate::workflow::PhaseRole::Evaluator => "evaluator",
                }
                .to_string(),
                gate: match &u.gate {
                    crate::workflow::GateSpec::Auto => "auto",
                    crate::workflow::GateSpec::HumanConfirm { .. } => "human_confirm",
                    crate::workflow::GateSpec::HumanConfirmIf(_) => "human_confirm_if",
                }
                .to_string(),
                skill_ref: u.skill_ref.clone(),
                has_validator_pin: u.validator.is_some(),
                executor_type: if u.tool_cmd.is_some() {
                    "tool"
                } else {
                    "agent"
                }
                .to_string(),
            },
        );
    }
    let mut pre = crate::pipeline::PreDistributed {
        session_id: run_id.to_string(),
        session: session.clone(),
        units: tail,
        clis: roster,
        workflow_id: session.workflow_id.clone(),
        cli_keys: session.clis.clone(),
    };
    let tail_dists = dists.into_iter().skip(cursor).collect();
    crate::pipeline::apply_distributions(store, &mut pre, tail_dists, &mut |ev| {
        emit(subscribers, ev)
    })?;
    // The cursor unit may be new (a late floor phase lands at the cursor): its first dispatch is
    // its own attempt 0, never the finished unit's.
    let mut s = crate::domain::get_session(&*store, run_id)?
        .ok_or_else(|| anyhow::anyhow!("run not found: {run_id}"))?;
    let units = crate::domain::session_units(&*store, run_id)?;
    s.attempt = units.get(s.unit_ix).map(super::next_attempt).unwrap_or(0);
    put_node(store, s.to_node())
}

#[cfg(test)]
#[path = "team_gate_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "replan_tests.rs"]
mod replan_tests;
