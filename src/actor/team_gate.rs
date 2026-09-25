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

/// (T5) After the fold: publish the unit-review `gate.decided`, and when the gate APPROVED a
/// teamed unit whose ledger pauses ([`tev::gate_pauses`]) mint and open the `team_dispute` gate.
/// Returns the pause prompt then: the caller pauses the run (the unit's work stands; the run
/// does not continue unattended). A denied unit is denied, never paused.
pub(super) fn decide_unit_review(
    session: &mut AgentSession,
    unit: &WorkUnit,
    attempt: u32,
    gate_id: &str,
    approved: bool,
) -> Option<String> {
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
    let team = session.team.get_or_insert_with(Default::default);
    team.gate_seq += 1;
    let dispute = tev::gate_id(&session.id, team.gate_seq);
    publish_fire(unit_event(
        &session.id,
        unit.ord,
        attempt,
        TeamBody::GateOpened(GateOpened {
            gate_id: dispute,
            kind: GateOpenedKind::TeamDispute {
                finding_ids: unresolved.clone(),
            },
        }),
    ));
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
    Some(format!(
        "Team dispute on unit {} ({}): {why}. The gate approved the work, but the run does not \
         continue unattended. Approve to continue, request changes to rework, or reject to cancel.",
        unit.ord, unit.description
    ))
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
                PendingStage::Publishing | PendingStage::Superseding
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
                        PendingStage::Publishing | PendingStage::Superseding
                    )
                });
        if waiting {
            return true;
        }
        let _ = reply.send(Ok(session.status));
        false
    });
}

#[cfg(test)]
#[path = "team_gate_tests.rs"]
mod tests;
