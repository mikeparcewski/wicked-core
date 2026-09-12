//! DISTRIBUTE — convene `wicked_council` IN-PROCESS to pick the CLI assigned to each unit.
//! Ported into COE from the retired wicked-agent. Each unit: convene the council over the seats
//! its skills admit ([`seat_candidates`], core#401 — the whole roster unless the unit invokes a
//! skill only a claude seat can be handed), read the verdict; the winner names the seat, else
//! gracefully degrade to the first candidate. Distribution ALWAYS yields an assignment for a unit
//! it seats; the ONE thing it refuses — at plan time, before any unit runs — is a roster with no
//! eligible seat for a unit's skills ([`crate::skills_snapshot::SkillsError::NoEligibleSeat`]).

use std::sync::Arc;

use wicked_council::dispatch::RealDispatcher;
use wicked_council::types::Dispatcher;
use wicked_council::{
    ids, work_kind_for, AgenticCli, CouncilTask, EstateHandle, EstateRankStore, Ledger,
    NoopEventSink, PollStatus, TaskState, Worker,
};

use crate::domain::{BenchedSeat, RoutingInfo, WorkUnit};
use crate::event::CoreEvent;
use crate::skills_snapshot::{SeatRequirement, SkillsError, SkillsSnapshot, NONPORTABLE_SEAT};

/// The production dispatcher — spawns real CLI subprocesses to collect council votes. Injected so
/// tests can substitute a deterministic stub (no subprocess, no flaky dispatch).
///
/// The budgets come from `RealDispatcher::from_env` rather than being written here. This function
/// used to hardcode 30 s for both, which is half the library's own default and below what any
/// shipped seat needs to answer a ballot — every seat was killed mid-reasoning and the council
/// degraded on 25 of 27 units (FINDING-026). A budget stated in one place cannot silently
/// contradict the one the library documents.
pub fn real_dispatcher() -> Arc<dyn Dispatcher + Send + Sync> {
    Arc::new(RealDispatcher::from_env())
}

/// Fans council lifecycle events back to the actor's single emit point (via
/// `Command::EmitEvent`), making deliberation visible to subscribers while a vote is
/// still in flight. `None` keeps the historical silent behaviour (tests, straight-line
/// pipeline callers).
pub type EventRelay = Arc<dyn Fn(CoreEvent) + Send + Sync>;

/// Adapts the council's string-keyed `EventSink` to run-scoped [`CoreEvent`]s. The council
/// worker emits `EV_COUNCIL_REQUESTED` when voters are polled, `EV_COUNCIL_DELIBERATED`
/// after each below-bar runoff ballot, and `EV_COUNCIL_VOTED` when the verdict lands; all
/// three are translated with the owning (session, ord) attached so the UI can pin them to
/// the unit being distributed. Rank bookkeeping (`EV_CLI_RANKED`) and any future council
/// event types are intentionally dropped — they are not run-scoped.
struct RelaySink {
    relay: EventRelay,
    session: String,
    ord: u32,
}

impl wicked_council::EventSink for RelaySink {
    fn emit(&self, event: &str, payload: &serde_json::Value) {
        let ev = match event {
            wicked_apps_core::EV_COUNCIL_REQUESTED => CoreEvent::CouncilConvened {
                session: self.session.clone(),
                ord: self.ord,
                clis: payload["clis"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default(),
            },
            wicked_apps_core::EV_COUNCIL_DELIBERATED => CoreEvent::CouncilDeliberated {
                session: self.session.clone(),
                ord: self.ord,
                round: payload["round"].as_u64().unwrap_or(0) as u32,
                agreement_pct: (payload["agreement_ratio"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0)
                    * 100.0)
                    .round() as u8,
                needed_pct: (payload["threshold"].as_f64().unwrap_or(0.0).clamp(0.0, 1.0) * 100.0)
                    .round() as u8,
                votes: payload["votes"].as_u64().unwrap_or(0) as u32,
            },
            wicked_apps_core::EV_COUNCIL_SEAT_FAILED => CoreEvent::CouncilSeatFailed {
                session: self.session.clone(),
                ord: self.ord,
                round: payload["round"].as_u64().unwrap_or(0) as u32,
                cli: payload["cli"].as_str().unwrap_or_default().to_string(),
                kind: payload["kind"].as_str().unwrap_or("unreported").to_string(),
                // `as_i64` is None for a JSON null (a seat that never reached exit), which is
                // exactly the case `exit_code: None` represents.
                exit_code: payload["exit_code"].as_i64().map(|c| c as i32),
                stderr: payload["stderr"].as_str().unwrap_or_default().to_string(),
                // F-031: the stdout tail and the classified cause (`null` when unclassified).
                stdout: payload["stdout"].as_str().unwrap_or_default().to_string(),
                detail: payload["detail"].as_str().unwrap_or_default().to_string(),
                reason: payload["reason"].as_str().map(str::to_string),
                // Separates a seat that never started from one that burned the whole budget —
                // the two look identical without it.
                latency_ms: payload["latency_ms"].as_u64().unwrap_or(0),
            },
            wicked_apps_core::EV_COUNCIL_VOTED => CoreEvent::CouncilVoted {
                session: self.session.clone(),
                ord: self.ord,
                consensus: payload["consensus"].as_bool().unwrap_or(false),
                agreement_pct: (payload["agreement_ratio"]
                    .as_f64()
                    .unwrap_or(0.0)
                    .clamp(0.0, 1.0)
                    * 100.0)
                    .round() as u8,
                votes: payload["votes"].as_u64().unwrap_or(0) as u32,
                // `map`, not `unwrap_or`: an absent key means the emitter reported no seat count,
                // and both candidate sentinels lie — `0` is an impossible denominator a consumer
                // could divide by, and `votes` would state that every seat answered. The live
                // emitter always sends it (same binary), so this only decides how a replayed or
                // hand-built payload reads.
                seated: payload["seated"].as_u64().map(|s| s as u32),
            },
            _ => return,
        };
        (self.relay)(ev);
    }
}

/// The distribution decision for one unit (positionally aligned with the input units).
#[derive(Debug, Clone)]
pub struct Distribution {
    pub assigned_cli: String,
    /// The assigned CLI's invocation template (so the runner can execute an ad-hoc CLI not in the
    /// registry). Resolved from the launch roster.
    pub assigned_invocation: Option<String>,
    pub council_task_ref: Option<String>,
    /// WHY this CLI won — the council verdict / ranking / degrade, made visible for the UI.
    pub routing: RoutingInfo,
    /// WHY the candidate seats were narrowed before the council voted (core#401) — the unit's
    /// skills admit only a claude seat — or `None` when every roster seat was a candidate. Rides
    /// [`CoreEvent::UnitDistributed`]`.seat_constraint`; `routing` reads exactly as before.
    pub seat_constraint: Option<String>,
    /// (F-7R2-006, wave 6) WHY the seats this unit was routed among were FEWER than the roster
    /// the launcher configured — `"N of M seats benched: codex (signed out — launcher), pi
    /// (not_logged_in — ballot)"` — or, for a `Degraded` routing, its own reason with that
    /// summary appended. `None` when every configured seat was eligible and the routing was not
    /// degraded. Rides `unitDistributed.degradedReason` for EVERY routing method (the Council arm
    /// used to emit `null` unconditionally — crew#533's anchor).
    pub degraded_reason: Option<String>,
    /// (F-7R2-006) The run-level bench set this distribution ran under — the launcher's unusable
    /// seats plus every seat whose ballot failed authentication — identical on every unit of one
    /// distribution; `apply_distributions` persists it on the session.
    pub benched: Vec<BenchedSeat>,
}

/// The invocation template for `key` from the launch roster (`None` if not found).
fn invocation_of(clis: &[AgenticCli], key: &str) -> Option<String> {
    clis.iter()
        .find(|c| c.key == key)
        .map(|c| c.headless_invocation.clone())
        .filter(|s| !s.trim().is_empty())
}

/// The distribution of a Tool-executor unit: the engine's own command, handed to no seat — its
/// `assigned_cli` is the tool's program token (the operator reads `wicked-estate`, not a seat
/// key), no invocation, no council, routing `tool`. The caller sets `benched` when a bench rides.
fn tool_distribution(unit: &WorkUnit) -> Distribution {
    Distribution {
        assigned_cli: unit
            .tool_cmd
            .as_ref()
            .and_then(|c| c.first())
            .cloned()
            .unwrap_or_else(|| "__tool__".to_string()),
        assigned_invocation: None,
        council_task_ref: None,
        routing: RoutingInfo::Tool,
        seat_constraint: None,
        degraded_reason: None,
        benched: Vec::new(),
    }
}

const DISTRIBUTE_CRITERIA: &[&str] = &["general"];

/// Convene the council (in-process) for every unit, persisting its task/verdict on the SHARED store
/// at `db_path` so council nodes land on the same file as the rest (R6). Units are dispatched in
/// parallel via `std::thread::scope`; each spawns its own in-memory council estate, so there is no
/// shared SQLite state and no concurrent-write hazard. (If `db_path` is `Some`, multiple threads
/// would open the same file — currently `db_path` is always `None` from the actor; callers passing
/// a file path should be aware of the SQLite single-writer constraint.)
pub fn distribute_units_on(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
    // The engine's own operational state home (`state_home::operational_home_of_db`) — fenced
    // for a claude BALLOT on its argv (wicked-crew#524 follow-up); `None` = the default candidate.
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<Vec<Distribution>> {
    distribute_units_on_benched(
        units,
        clis,
        session_id,
        db_path,
        dispatcher,
        relay,
        operational_home,
        &[],
    )
}

/// [`distribute_units_on`] for a run that already BENCHED seats (F-7R2-006): `prior_benched` —
/// the session's persisted bench set (a resume, a re-plan) — is honoured beside the roster's
/// own health verdicts; a benched seat is never convened.
#[allow(clippy::too_many_arguments)]
pub fn distribute_units_on_benched(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
    operational_home: Option<&std::path::Path>,
    prior_benched: &[BenchedSeat],
) -> anyhow::Result<Vec<Distribution>> {
    // core#401: the skills root the seats are judged against. Resolved ONLY when a seated unit
    // names a skill — a skill-free run consults no ladder and logs no fallback line, exactly as
    // its launches would not — and never a refusal of its own: absent, failed or misconfigured,
    // the routing is unconstrained and the launch admission decides at the first unit, as today.
    let names_a_skill = units
        .iter()
        .any(|u| u.tool_cmd.is_none() && u.skill_ref.as_deref().is_some_and(|r| !r.is_empty()));
    let snapshot = if names_a_skill {
        crate::skills_snapshot::routing_snapshot()
    } else {
        None
    };
    distribute_units_against_benched(
        units,
        clis,
        session_id,
        db_path,
        dispatcher,
        relay,
        snapshot.as_ref(),
        operational_home,
        prior_benched,
    )
}

/// The program token of a seat's `headless_invocation` — the SAME carrier identity the council
/// dispatch judges (`run_in_isolation` execs the first token and hands it to
/// `seat_config_for_carrier`): a double-quoted program or the first bare token.
fn ballot_program(invocation: &str) -> &str {
    let s = invocation.trim_start();
    match s.strip_prefix('"') {
        Some(rest) => rest.split('"').next().unwrap_or(""),
        None => s.split_whitespace().next().unwrap_or(""),
    }
}

/// Is this seat a claude BALLOT — judged exactly as the council will exec it: the invocation's
/// program token through the one shared carrier resolver (`binary_is_claude`; case rules follow
/// the OS's own executable lookup), never the roster key or the record's `binary`.
fn is_claude_ballot(cli: &AgenticCli) -> bool {
    wicked_apps_core::spawn::binary_is_claude(ballot_program(&cli.headless_invocation))
}

/// The roster the council convenes with: every claude ballot's `trust_flags` gain
/// `--disallowedTools <state-home rules>` (`execute_wrapped::ballot_deny_rules`) — the half of the
/// fence the shared worker file omits by design; every other seat is handed back untouched.
fn fenced_roster(
    clis: &[AgenticCli],
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<Vec<AgenticCli>> {
    let rules =
        crate::execute_wrapped::ballot_deny_rules(operational_home).map_err(anyhow::Error::msg)?;
    Ok(clis
        .iter()
        .cloned()
        .map(|mut c| {
            if is_claude_ballot(&c) && !rules.is_empty() {
                c.trust_flags.push("--disallowedTools".into());
                c.trust_flags.push(rules.join(","));
            }
            c
        })
        .collect())
}

/// The candidate seats for one unit: the roster keys and records the council votes among, and
/// WHY they were narrowed — or `None` when every roster seat is a candidate.
type Candidates = Option<(Vec<AgenticCli>, String)>;

/// [`distribute_units_on`] against an explicit skills root (`None` ⇒ no seat is constrained), so
/// the routing is testable without the process environment — the same split the admission has
/// (`resolve_ladder` / `resolve_ladder_in`).
#[allow(clippy::too_many_arguments)] // the routing seam is testable without the process env
#[cfg(test)]
pub(crate) fn distribute_units_against(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
    snapshot: Option<&SkillsSnapshot>,
    operational_home: Option<&std::path::Path>,
) -> anyhow::Result<Vec<Distribution>> {
    distribute_units_against_benched(
        units,
        clis,
        session_id,
        db_path,
        dispatcher,
        relay,
        snapshot,
        operational_home,
        &[],
    )
}

/// The seats of `clis` a run may still route to: not in `benched`, and not declared unusable by
/// the launcher's health probe ([`AgenticCli::health`]). Pure; the ONE eligibility rule the
/// council roster, the evaluator≠creator reassignment, the failover ladder and the judge/triage
/// seat selection all read (F-7R2-006).
pub(crate) fn eligible_seats<'a>(
    clis: &'a [AgenticCli],
    benched: &[BenchedSeat],
) -> Vec<&'a AgenticCli> {
    clis.iter()
        .filter(|c| !benched.iter().any(|b| b.cli == c.key))
        .filter(|c| c.health.as_ref().is_none_or(|h| h.usable))
        .collect()
}

/// (F-7R2-006) The launcher-declared bench: every seat whose roster record says
/// `health.usable == false`, with the launcher's reason.
pub(crate) fn launcher_benched(clis: &[AgenticCli]) -> Vec<BenchedSeat> {
    clis.iter()
        .filter_map(|c| {
            let h = c.health.as_ref()?;
            (!h.usable).then(|| BenchedSeat {
                cli: c.key.clone(),
                reason: h
                    .reason
                    .clone()
                    .unwrap_or_else(|| "unusable (launcher health probe)".to_string()),
                source: "launcher".to_string(),
            })
        })
        .collect()
}

/// The routing core, honouring a bench set (F-7R2-006). Rules, in order:
///
/// 0. SEAT REQUIREMENT is per UNIT (F-E2E-011) — a Tool-executor unit (`tool_cmd`) is the engine's
///    own command: it convenes no council and is handed to no seat. A plan whose EVERY unit is a
///    tool therefore needs no seat at all and is routed `tool` before any eligibility verdict —
///    crew launches such a run (`onboarding`: index + annotate) with `clis: []` by design
///    (wicked-crew#533). Rules 1–3 apply once at least one planned unit needs a seat.
/// 1. ELIGIBILITY — the seats the launcher declared unusable (`health.usable == false`) and every
///    seat in `prior_benched` are set aside; the council convenes over the rest (so
///    `councilConvened.clis` names only eligible seats). An empty eligible set REFUSES the plan by
///    name — better than five human gates on dead seats.
/// 2. BALLOT AUTHENTICATION — a seat whose council ballot failed with `not_logged_in` is benched
///    for the run the moment the councils return; a unit the council handed to such a seat is
///    reassigned to the first still-eligible seat its skills admit (routing `degraded`, naming
///    both seats), and the evaluator≠creator reassignment picks only among still-eligible seats.
/// 3. `degraded_reason` names the bench on EVERY unit whenever eligible < configured, and the
///    whole bench rides each `Distribution` for the actor to persist.
#[allow(clippy::too_many_arguments)]
pub(crate) fn distribute_units_against_benched(
    units: &[WorkUnit],
    configured: &[AgenticCli],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
    snapshot: Option<&SkillsSnapshot>,
    operational_home: Option<&std::path::Path>,
    prior_benched: &[BenchedSeat],
) -> anyhow::Result<Vec<Distribution>> {
    let mut benched: Vec<BenchedSeat> = prior_benched.to_vec();
    for seat in launcher_benched(configured) {
        crate::domain::bench_seat(&mut benched, seat);
    }
    // (F-E2E-011, rule 0) No planned unit needs a seat ⇒ nothing to elect and nothing to refuse:
    // every unit is routed `tool` here, before the eligibility verdict below can read an empty
    // roster as "every configured seat is benched" (a 1-second `sessionFailed` on every
    // onboarding run, with a "sign a seat in" remedy for a run that seats nobody). The bench still
    // rides every distribution, so a launcher-declared unusable seat is persisted exactly as it
    // would be for a seated plan.
    if units.iter().all(|u| u.tool_cmd.is_some()) {
        return Ok(units
            .iter()
            .map(|u| Distribution {
                benched: benched.clone(),
                ..tool_distribution(u)
            })
            .collect());
    }
    let eligible: Vec<AgenticCli> = eligible_seats(configured, &benched)
        .into_iter()
        .cloned()
        .collect();
    if eligible.is_empty() {
        anyhow::bail!(
            "no eligible seat for {session_id}: every configured seat is benched — {} (sign a \
             seat in, or add one; a council over dead seats would only park the run at a human \
             gate per unit)",
            crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
        );
    }
    if benched.len() > prior_benched.len() {
        eprintln!(
            "wicked-core: distribution for {session_id} convenes {} of {} configured seats — {}",
            eligible.len(),
            configured.len(),
            crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
        );
    }
    let clis: &[AgenticCli] = &eligible;
    // Plan-time refusal (core#401): a unit whose skills only a claude seat can be handed, on a
    // roster with none, is refused HERE — before any council convenes and before any unit does
    // work — naming the skill, its portability and the seat kind required. The ladder would have
    // refused the same unit by name at launch; by then work may have been done and the escalation
    // gate cannot retarget a seat, so the run could only be cancelled.
    // wicked-crew#524 follow-up (review on core#436): the council's claude seat runs under the
    // worker home with the trust flag appended (`wicked_council::dispatch::run_in_isolation`) and
    // creates no fence of its own — a council convened before any worker had spawned used to run
    // with no deny fence at all. Fence the ROSTER here, before any ballot, in the two halves
    // `execute_wrapped::ballot_deny_rules` documents: the shared worker `settings.json` (the same
    // idempotent writer the ACP spawn uses) and, on the claude seat's own argv as
    // `--disallowedTools` (the council appends `trust_flags` verbatim), the state-home rules that
    // file omits by design. A fence that cannot be written refuses the council — fail closed,
    // exactly as the ACP spawn refuses the worker. A roster with no claude ballot is untouched.
    let fenced: Vec<AgenticCli>;
    let clis: &[AgenticCli] = if clis.iter().any(is_claude_ballot) {
        crate::acp_runner::ensure_shared_worker_fence().map_err(|e| {
            anyhow::anyhow!(
                "council for {session_id}: the shared worker fence (<worker home>/claude/\
                 settings.json) could not be written ({e}); refusing to convene a claude seat \
                 without its deny fence"
            )
        })?;
        fenced = fenced_roster(clis, operational_home).map_err(|e| {
            anyhow::anyhow!(
                "council for {session_id}: the ballot's state-home fence could not be built ({e}); \
                 refusing to convene a claude seat without it"
            )
        })?;
        &fenced
    } else {
        clis
    };
    let candidates = seat_candidates(units, clis, snapshot)?;
    let roster_keys: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    let routed: Vec<(Distribution, Vec<String>)> = std::thread::scope(|s| {
        // Spawn all units concurrently. Scoped-thread closures borrow from the enclosing
        // scope — `std::thread::scope` guarantees all threads finish before it returns,
        // making the borrows sound without requiring `move`.
        let relay = &relay;
        let candidates = &candidates;
        let roster_keys = &roster_keys;
        let handles: Vec<_> = units
            .iter()
            .zip(candidates.iter())
            .map(|(unit, candidates)| {
                s.spawn(move || {
                    if unit.tool_cmd.is_some() {
                        Ok((tool_distribution(unit), Vec::new()))
                    } else {
                        // The council votes among the CANDIDATES — the whole roster, or the seats
                        // the unit's skills admit. A single candidate takes the single-seat path
                        // below (a truthful 1-of-1 verdict, no ballot), so a Claude-only unit on a
                        // roster with one claude seat convenes nothing.
                        let (unit_clis, unit_keys, constraint) = match candidates {
                            Some((eligible, why)) => (
                                eligible.as_slice(),
                                eligible.iter().map(|c| c.key.clone()).collect::<Vec<_>>(),
                                Some(why.clone()),
                            ),
                            None => (clis, roster_keys.clone(), None),
                        };
                        distribute_one(
                            unit,
                            unit_clis,
                            &unit_keys,
                            session_id,
                            db_path,
                            dispatcher,
                            relay.clone(),
                        )
                        .map(|(d, auth_failed)| {
                            (
                                Distribution {
                                    seat_constraint: constraint,
                                    ..d
                                },
                                auth_failed,
                            )
                        })
                    }
                })
            })
            .collect();
        // Join ALL handles before inspecting results. Early-returning on the first join error
        // would drop remaining handles, letting the scope re-propagate their panics and
        // bypass the intended `anyhow::Error` mapping.
        let results: Vec<_> = handles.into_iter().map(|h| h.join()).collect();
        results
            .into_iter()
            .map(|r| {
                r.map_err(|e| {
                    let msg = e
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| e.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "council thread panicked".to_string());
                    anyhow::anyhow!(msg)
                })
                .and_then(|r| r)
            })
            .collect::<anyhow::Result<_>>()
    })?;
    // (F-7R2-006 rule 2) A seat whose ballot failed AUTHENTICATION is benched for the run — every
    // later routing decision in this distribution (and, persisted, every dispatch) skips it.
    let mut dists: Vec<Distribution> = Vec::with_capacity(routed.len());
    for (dist, auth_failed) in routed {
        for cli in auth_failed {
            if crate::domain::bench_seat(
                &mut benched,
                BenchedSeat {
                    cli: cli.clone(),
                    reason: wicked_council::types::SeatFailureReason::NotLoggedIn
                        .as_str()
                        .to_string(),
                    source: "ballot".to_string(),
                },
            ) {
                eprintln!(
                    "wicked-core: seat '{cli}' failed authentication on a council ballot for \
                     {session_id}; benched for the run (F-7R2-006)"
                );
            }
        }
        dists.push(dist);
    }
    let still_eligible: Vec<String> = eligible_seats(clis, &benched)
        .into_iter()
        .map(|c| c.key.clone())
        .collect();
    if still_eligible.is_empty() {
        anyhow::bail!(
            "no eligible seat for {session_id}: every seat failed authentication on its council \
             ballot — {}",
            crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
        );
    }
    // A unit the council handed to a seat that then failed authentication is moved to the
    // first still-eligible seat its skills admit — routing `degraded`, naming both seats.
    for ((unit, dist), candidates) in units.iter().zip(dists.iter_mut()).zip(candidates.iter()) {
        if unit.tool_cmd.is_some() || still_eligible.contains(&dist.assigned_cli) {
            continue;
        }
        let admits = |k: &String| match candidates {
            Some((eligible, _)) => eligible.iter().any(|c| &c.key == k),
            None => true,
        };
        let Some(alt) = still_eligible.iter().find(|k| admits(k)) else {
            anyhow::bail!(
                "unit {} of {session_id} cannot be seated: the council picked '{}', which failed \
                 authentication on its ballot, and no still-eligible seat its skills admit \
                 remains ({})",
                unit.ord,
                dist.assigned_cli,
                crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
            );
        };
        let was = std::mem::replace(&mut dist.assigned_cli, alt.clone());
        dist.assigned_invocation = invocation_of(clis, alt);
        dist.council_task_ref = None;
        dist.routing = RoutingInfo::Degraded {
            reason: format!(
                "council picked '{was}', which failed authentication on its ballot \
                 (not_logged_in); reassigned to '{alt}'"
            ),
        };
    }
    enforce_evaluator_distinct(units, &mut dists, &still_eligible, clis, &candidates);
    // (F-7R2-006 rule 3) `degradedReason` on EVERY unit whenever eligible < configured.
    let summary = crate::domain::benched_summary(&benched, configured.len());
    for d in &mut dists {
        d.benched = benched.clone();
        d.degraded_reason = match &d.routing {
            RoutingInfo::Tool => None,
            RoutingInfo::Degraded { reason } => Some(match &summary {
                Some(s) => format!("{reason}; {s}"),
                None => reason.clone(),
            }),
            RoutingInfo::Council { .. } | RoutingInfo::EvaluatorDistinct { .. } => summary.clone(),
        };
    }
    Ok(dists)
}

/// The candidate seats of every unit (positionally aligned), narrowed by what its skills admit
/// (core#401): a unit whose `skill_ref` — or a transitive mandate — is `portable: false` in the
/// handed snapshot, or whose root is the Claude-only live-cache fallback, may be seated only on a
/// claude seat ([`crate::skills_snapshot::seat_requirement`]); every other unit, and every tool
/// unit, is unconstrained. A roster with NO eligible seat for such a unit is refused here, by
/// name — the plan-time half of the refusal `admit_refs` would otherwise make at launch.
fn seat_candidates(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    snapshot: Option<&SkillsSnapshot>,
) -> Result<Vec<Candidates>, SkillsError> {
    let Some(snapshot) = snapshot else {
        return Ok(units.iter().map(|_| None).collect());
    };
    // The eligible claude seats depend on the roster alone, never on the unit, so they are judged
    // ONCE per call: the first Claude-only unit resolves every seat on both carriers (each a read
    // of the merged registry) and every later one reuses that verdict — not once per unit per
    // seat, so a `clis.toml` that changes under one distribution cannot hand two of its units two
    // different rosters (Copilot, #402 review pass 4).
    let mut eligible_claude: Option<Vec<AgenticCli>> = None;
    units
        .iter()
        .map(|u| {
            if u.tool_cmd.is_some() {
                return Ok(None);
            }
            match crate::skills_snapshot::seat_requirement(snapshot, u.skill_ref.as_deref()) {
                SeatRequirement::Any => Ok(None),
                SeatRequirement::ClaudeOnly { skills, why } => {
                    let eligible = eligible_claude
                        .get_or_insert_with(|| {
                            clis.iter()
                                .filter(|c| seat_is_claude(clis, &c.key))
                                .cloned()
                                .collect()
                        })
                        .clone();
                    if eligible.is_empty() {
                        return Err(SkillsError::NoEligibleSeat {
                            ord: u.ord,
                            skills,
                            required_seat: NONPORTABLE_SEAT,
                            roster: clis.iter().map(|c| c.key.clone()).collect(),
                            why,
                        });
                    }
                    Ok(Some((eligible, why)))
                }
            }
        })
        .collect()
}

#[cfg(test)]
thread_local! {
    /// Test-only: how many seat judgements [`seat_is_claude`] has made on THIS thread.
    /// [`seat_candidates`] judges on its caller's thread, so a test reads exactly its own pass —
    /// each roster seat once, not once per unit per seat.
    static SEAT_JUDGEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Is the roster seat `key` one the delivery can hand a NON-PORTABLE skill to — a claude seat on
/// BOTH carriers (design v3.2 §3)? Judged by the SAME resolutions the two runners make at launch
/// (#402 review pass 2), never by the roster record's own fields: the ACP carrier reloads the
/// MERGED registry by key and judges that record's `binary` (`acp_runner::acp_seat_identity`);
/// the wrapped carrier judges the first token of the template the unit will carry — this
/// roster's template for the key (its `assigned_invocation`), else the registry's, else the key
/// (`execute_wrapped::wrapped_seat_identity`). Both must say claude: the routing cannot know
/// which carrier a launch takes (ACP first, the wrapped runner as its fallback), and a seat the
/// operator's `clis.toml` re-points at another carrier — or a roster record keyed `claude` whose
/// template runs something else — is exactly the seat the ladder would refuse mid-run. A seat the
/// two carriers would disagree about is therefore refused loudly at plan time, not seated.
fn seat_is_claude(clis: &[AgenticCli], key: &str) -> bool {
    use crate::skills_snapshot::WorkerCli;
    #[cfg(test)]
    SEAT_JUDGEMENTS.with(|n| n.set(n.get() + 1));
    let acp = crate::acp_runner::acp_seat_identity(key);
    let wrapped = crate::execute_wrapped::wrapped_seat_identity(key, invocation_of(clis, key));
    matches!(acp, WorkerCli::Claude) && matches!(wrapped, WorkerCli::Claude)
}

/// METHODOLOGY: evaluator ≠ creator. A REVIEW/TEST unit must not run on a CLI that produced the work
/// it checks, so after distribution we reassign any review/test unit whose council-picked CLI matches
/// a build/recon CLI to a roster seat NOT used for building (when the roster has the seats to do so)
/// — a seat the unit's skills ADMIT (core#401): a Claude-only review unit is never moved onto a seat
/// the ladder would refuse it on; with no such alternative it stays where the council put it.
fn enforce_evaluator_distinct(
    units: &[WorkUnit],
    dists: &mut [Distribution],
    roster_keys: &[String],
    clis: &[AgenticCli],
    candidates: &[Candidates],
) {
    use crate::domain::StageKind;
    let builder_clis: std::collections::HashSet<String> = units
        .iter()
        .zip(dists.iter())
        .filter(|(u, _)| matches!(u.stage, StageKind::Build | StageKind::Recon))
        .map(|(_, d)| d.assigned_cli.clone())
        .collect();
    if roster_keys.len() < 2 || builder_clis.is_empty() {
        return; // can't distinguish with one seat / nothing built
    }
    // Warn when every roster seat is a builder CLI so operators can detect degraded separation.
    // `find` below will return `None` for every Review/Test unit in this configuration, leaving
    // them on their original (builder) CLI with no routing change — silently, unless we speak up.
    let has_evaluator_seat = roster_keys.iter().any(|k| !builder_clis.contains(k));
    if !has_evaluator_seat {
        let review_test_affected = units.iter().zip(dists.iter()).any(|(u, d)| {
            u.tool_cmd.is_none()
                && matches!(u.stage, StageKind::Review | StageKind::Test)
                && builder_clis.contains(&d.assigned_cli)
        });
        if review_test_affected {
            eprintln!(
                "wicked-core: evaluator\u{2260}creator separation cannot be enforced: all roster \
                 seats are assigned Build/Recon phases; Review/Test phases will use builder CLIs. \
                 Consider adding a dedicated evaluator seat."
            );
        }
    }
    for ((u, d), candidates) in units.iter().zip(dists.iter_mut()).zip(candidates.iter()) {
        if u.tool_cmd.is_some() {
            continue; // Tool phases have no CLI to distinct
        }
        if matches!(u.stage, StageKind::Review | StageKind::Test)
            && builder_clis.contains(&d.assigned_cli)
        {
            let admits = |k: &String| match candidates {
                Some((eligible, _)) => eligible.iter().any(|c| &c.key == k),
                None => true,
            };
            if let Some(alt) = roster_keys
                .iter()
                .find(|k| !builder_clis.contains(*k) && admits(k))
            {
                let was = std::mem::replace(&mut d.assigned_cli, alt.clone());
                d.assigned_invocation = invocation_of(clis, alt);
                d.routing = RoutingInfo::EvaluatorDistinct {
                    winner: alt.clone(),
                    was,
                };
            }
        }
    }
}

/// Route one unit. Returns the distribution AND the seats whose ballot failed AUTHENTICATION
/// (`SeatFailureReason::NotLoggedIn`) — the caller benches them for the run (F-7R2-006).
fn distribute_one(
    unit: &WorkUnit,
    clis: &[AgenticCli],
    roster_keys: &[String],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
) -> anyhow::Result<(Distribution, Vec<String>)> {
    // FINDING-010: a single-seat roster has nothing to elect. Convening a council here still queues a
    // ballot and dispatches it to the sole CLI — a real subprocess turn spent asking one voter to pick
    // the one option, ~30s of dead wall-clock per unit for a foregone conclusion (and a councilConvened
    // the operator must then read past). Short-circuit: assign the only seat directly and record a
    // TRUTHFUL one-seat verdict (1 of 1, 100% agreement, no dissent). No ballot, no dispatch, no
    // council estate — the dispatcher is never touched. `enforce_evaluator_distinct` is a no-op at
    // len < 2, so nothing downstream depends on the council having run here.
    if let [only] = clis {
        return Ok((
            Distribution {
                assigned_invocation: invocation_of(clis, &only.key),
                assigned_cli: only.key.clone(),
                council_task_ref: None,
                routing: RoutingInfo::Council {
                    winner: only.key.clone(),
                    agreement_pct: 100,
                    returned: 1,
                    seated: Some(1),
                    dissent: 0,
                },
                seat_constraint: None,
                degraded_reason: None,
                benched: Vec::new(),
            },
            Vec::new(),
        ));
    }

    let estate = match db_path {
        Some(path) => EstateHandle::new(
            wicked_apps_core::SqliteStore::open(path)
                .map_err(|e| anyhow::anyhow!("open council estate on {path}: {e}"))?,
        ),
        None => EstateHandle::in_memory()
            .map_err(|e| anyhow::anyhow!("open council estate handle: {e}"))?,
    };
    let ledger = Ledger::new(estate.clone());
    let rank_store = Arc::new(EstateRankStore::new(estate));
    // Council lifecycle events flow to the actor's emit point when a relay is armed;
    // otherwise deliberation stays silent (the pre-relay behaviour).
    let events: Arc<dyn wicked_council::EventSink + Send + Sync> = match relay {
        Some(relay) => Arc::new(RelaySink {
            relay,
            session: session_id.to_string(),
            ord: unit.ord,
        }),
        None => Arc::new(NoopEventSink),
    };

    // NOTE: a historical-ranking fast path once lived here, but distribution always runs with an
    // IN-MEMORY council estate — the single-writer actor owns the only shared-store handle, so we
    // cannot open a second writable one here (`db_path` is always `None` from the pipeline). Rankings
    // therefore never persist across runs, so the fast path could never fire; it was removed rather
    // than ship a `RoutingInfo::Ranked` mode the engine can't actually produce. Every unit convenes.
    let criteria: Vec<String> = DISTRIBUTE_CRITERIA.iter().map(|s| s.to_string()).collect();
    let work_kind = work_kind_for(&criteria);
    let worker = Worker::new(
        ledger,
        dispatcher.clone(),
        rank_store,
        events,
        clis.to_vec(),
        work_kind,
    );

    // Build numbered capability profiles — CLI names are NEVER exposed to voters.
    // Each voter sees only the capability description and picks a number, preventing
    // self-selection bias (a CLI knowing its own name will recommend itself).
    let cap_map: Vec<(String, String)> = clis
        .iter()
        .map(|c| {
            let label = c
                .capabilities
                .as_deref()
                .unwrap_or(&c.display_name)
                .to_string();
            (label, c.key.clone())
        })
        .collect();
    let option_labels: Vec<String> = cap_map.iter().map(|(label, _)| label.clone()).collect();

    let task = CouncilTask {
        id: ids::new_task_id(),
        topic: format!(
            "A software task needs an agent to execute it.\n\
             Task description: {}\n\
             Which numbered capability profile is the best fit?",
            unit.description
        ),
        options: option_labels,
        criteria,
        session_id: session_id.to_string(),
    };
    let task_id = worker.queue_blocking(task);
    let status: Option<PollStatus> = worker.poll(&task_id);
    let auth_failed = status
        .as_ref()
        .map(ballot_auth_failures)
        .unwrap_or_default();
    let (assigned_cli, routing) = route_from_status(status.as_ref(), roster_keys, &cap_map);

    Ok((
        Distribution {
            assigned_invocation: invocation_of(clis, &assigned_cli),
            assigned_cli,
            council_task_ref: Some(task_id),
            routing,
            seat_constraint: None,
            degraded_reason: None,
            benched: Vec::new(),
        },
        auth_failed,
    ))
}

/// The seats whose ballot on this council failed AUTHENTICATION — classified by the council from
/// the seat's own words (`Not logged in`, `Authentication required`, `401`, …:
/// `SeatFailureReason::classify`). Deduplicated, in first-seen order.
fn ballot_auth_failures(status: &PollStatus) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in &status.seat_failures {
        if f.failure.reason == Some(wicked_council::types::SeatFailureReason::NotLoggedIn)
            && !out.contains(&f.cli)
        {
            out.push(f.cli.clone());
        }
    }
    out
}

/// Clamp a `0.0..=1.0` ratio to an integer percent (keeps the domain `Eq`).
fn pct(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 100.0).round() as u8
}

/// Why a council in a non-`Voted` state produced nothing, as specifically as the record allows.
///
/// This replaces the single string `"council did not reach a vote"`, which was emitted for
/// `TimedOut`, `Failed`, `Queued` and `Running` alike and named no seat. A campaign of 27
/// convened councils degraded 25 times, every one of them reporting that same sentence — there
/// was nothing in it to act on.
///
/// Three levels, most specific first: the seats' own reported failures; failing that, the
/// council's OWN failure (a panic in synthesis or emission belongs to no seat, so it has no
/// entry in `seat_failures` — without this it reproduced the same undiagnosable string the
/// finding exists to eliminate, FINDING-026 E); failing that, the lifecycle state (which at
/// least distinguishes "ran out of time" from "could not start" from "still running when
/// polled"). The state is always named so the levels are never confused.
fn no_vote_reason(status: &PollStatus) -> String {
    let state = match status.state {
        TaskState::Queued => "never started (still queued when polled)",
        TaskState::Running => "still running when polled",
        TaskState::TimedOut => "no seat returned a vote",
        TaskState::Failed => "could not run",
        // `Voted` does not reach here — the caller checks for it first.
        TaskState::Voted => "reported a vote but was not in the voted state",
    };

    if status.seat_failures.is_empty() {
        return match &status.failure_detail {
            Some(detail) => format!("council {state} — the council itself failed: {detail}"),
            None => format!("council {state} (no per-seat reason recorded)"),
        };
    }

    let seats: Vec<String> = status
        .seat_failures
        .iter()
        .map(|f| format!("{}: {}", f.cli, f.failure.summary()))
        .collect();
    let seats = format!("council {state} — {}", seats.join("; "));
    // Both can be present: seats failed AND the council then unwound. Neither explains the
    // other, so neither is dropped.
    match &status.failure_detail {
        Some(detail) => format!("{seats}; the council itself failed: {detail}"),
        None => seats,
    }
}

/// Resolve the assigned CLI from the council's poll status AND the routing provenance.
///
/// Voters respond with a **capability-profile number** (e.g. "2"), never a CLI name.
/// We parse the leading integer from the winning recommendation, use it as a 1-based
/// index into `cap_map` (ordered `(capability_label, cli_key)`), and fall back
/// gracefully to the first seat if the number is missing or out of range.
fn route_from_status(
    status: Option<&PollStatus>,
    roster_keys: &[String],
    cap_map: &[(String, String)],
) -> (String, RoutingInfo) {
    let fallback = || {
        roster_keys
            .first()
            .cloned()
            .unwrap_or_else(|| "claude".to_string())
    };
    let degrade = |reason: &str| {
        (
            fallback(),
            RoutingInfo::Degraded {
                reason: reason.to_string(),
            },
        )
    };

    let Some(status) = status else {
        return degrade("council returned no status");
    };
    if status.state != TaskState::Voted {
        return degrade(&no_vote_reason(status));
    }
    let Some(verdict) = &status.verdict else {
        return degrade("council produced no verdict");
    };
    let Some(winner) = &verdict.winning_recommendation else {
        return degrade("verdict named no winner");
    };

    // Parse the leading integer from the recommendation text (voters are told to lead with
    // the option number). "2 — broad reasoning..." → 2 → index 1.
    let idx_opt = winner
        .trim()
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .and_then(|tok| tok.parse::<usize>().ok())
        .filter(|&n| n >= 1 && n <= cap_map.len())
        .map(|n| n - 1); // convert to 0-based

    if let Some(idx) = idx_opt {
        let seat = cap_map[idx].1.clone();
        // Confirm the seat exists in the roster (cap_map may be a superset if a CLI was
        // added after roster construction — degrade rather than assign an unknown key).
        if roster_keys.iter().any(|k| k == &seat) {
            return (
                seat.clone(),
                RoutingInfo::Council {
                    winner: seat,
                    agreement_pct: pct(verdict.agreement_ratio),
                    returned: status.returned,
                    // Off the STATUS, not the verdict: the verdict's own `seated` degrades to the
                    // cast count when it was not recorded, while the status counts the seats the
                    // ledger actually convened. They agree on every live path; where they don't,
                    // the ledger is the one that observed the council.
                    seated: Some(status.seated),
                    dissent: verdict.dissent.len() as u32,
                },
            );
        }
    }

    degrade(&format!(
        "recommendation '{winner}' did not resolve to a roster seat"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wicked_council::types::{Category, Confidence, InputMode, Vote};
    use wicked_council::{CouncilTask, Verdict};

    /// Counts every `dispatch` — a ballot dispatched to a CLI is a real subprocess turn. A
    /// short-circuited single-seat roster must never reach it.
    struct SpyDispatcher {
        calls: Arc<AtomicUsize>,
    }
    impl Dispatcher for SpyDispatcher {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Some(Vote {
                cli: cli.key.clone(),
                // "1 …" resolves to option 1 so the >1-seat control path reaches a real verdict.
                recommendation: "1 — fit".into(),
                top_risk: "none".into(),
                change_my_mind: "no".into(),
                disqualifier: None,
                confidence: Confidence::default(),
                provenance: "spy".into(),
            })
        }
    }

    fn seat(key: &str) -> AgenticCli {
        AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: format!("run-{key} {{PROMPT}}"),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            acp: None,
            capabilities: Some(format!("{key} capabilities")),
            login_invocation: None,
            health: None,
        }
    }

    /// FINDING-010: a single-seat roster is assigned WITHOUT convening a council — the dispatcher is
    /// never called (no ballot subprocess), and the routing is a truthful 1-of-1 verdict. The 2-seat
    /// control proves the guard is scoped to len==1: there, the council DOES convene (dispatcher hit).
    /// Mutation: delete the `if let [only] = clis` short-circuit and the single-seat case dispatches
    /// (calls > 0), failing the first assertion.
    /// Records every seat it is asked to dispatch — what the council actually convened with.
    struct RecordingDispatcher {
        seen: Arc<std::sync::Mutex<Vec<AgenticCli>>>,
    }
    impl Dispatcher for RecordingDispatcher {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            self.seen.lock().unwrap().push(cli.clone());
            Some(Vote {
                cli: cli.key.clone(),
                recommendation: "1 — fit".into(),
                top_risk: "none".into(),
                change_my_mind: "no".into(),
                disqualifier: None,
                confidence: Confidence::default(),
                provenance: "recording".into(),
            })
        }
    }

    /// A seat whose INVOCATION execs `claude` — the carrier identity the council judges.
    fn claude_seat(key: &str) -> AgenticCli {
        let mut c = seat(key);
        c.binary = "claude".into();
        c.headless_invocation = "claude -p {PROMPT}".into();
        c.trust_flags = vec!["--dangerously-skip-permissions".into()];
        c
    }

    /// wicked-crew#524 follow-up (review on core#436): a claude BALLOT is judged by the
    /// invocation's program token through the shared carrier resolver — exactly what the council
    /// execs — never by the roster key or the record's `binary`.
    #[test]
    fn a_claude_ballot_is_judged_by_the_invocations_program_token_like_the_council() {
        let mut custom = seat("custom");
        custom.headless_invocation = "claude -p {PROMPT}".into();
        assert!(
            is_claude_ballot(&custom),
            "a custom key whose template execs claude IS a ballot"
        );
        let mut quoted = seat("q");
        quoted.headless_invocation = "\"/opt/tools/claude\" -p {PROMPT}".into();
        assert!(
            is_claude_ballot(&quoted),
            "a quoted program path is judged by its file stem"
        );
        let by_key_only = seat("claude"); // template `run-claude {PROMPT}`, binary `unused`
        assert!(
            !is_claude_ballot(&by_key_only),
            "the key alone is not the carrier"
        );
        let mut binary_only = seat("x");
        binary_only.binary = "claude".into();
        assert!(
            !is_claude_ballot(&binary_only),
            "the record's binary alone is not the carrier"
        );
        assert_eq!(ballot_program("  codex exec {PROMPT}"), "codex");
        assert_eq!(ballot_program("\"/a b/claude\" -p"), "/a b/claude");
        assert_eq!(ballot_program(""), "");
    }

    /// wicked-crew#524 follow-up (review on core#436): convening a council that seats a claude
    /// ballot writes the SHARED worker fence first AND hands the claude seat the state-home rules
    /// that file omits — on its own argv (`--disallowedTools`), the operational home included —
    /// so a council-first ballot is fenced like a worker launch. Every other seat is untouched; a
    /// claude-less roster writes nothing. (The test process arms `WICKED_WORKER_HOME` to a
    /// per-process temp home pre-main; this test re-aims it at a fixture and restores it.)
    #[test]
    fn convening_a_claude_ballot_writes_the_shared_fence_and_fences_the_state_home_on_argv() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let hatch = crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV;
        let prev_hatch = std::env::var_os(hatch);
        std::env::remove_var(hatch);
        let base = std::env::temp_dir().join(format!("wdistribute-fence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &base);
        let op_home = base.join("state");
        let unit = WorkUnit::pending("u1", "s1", 0, "Write the parser module");
        let seen: Arc<std::sync::Mutex<Vec<AgenticCli>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher {
            seen: Arc::clone(&seen),
        });
        // No claude ballot ⇒ nothing is written, no seat is touched.
        distribute_units_on(
            std::slice::from_ref(&unit),
            &[seat("codex"), seat("pi")],
            "s1",
            None,
            &dispatcher,
            None,
            Some(&op_home),
        )
        .expect("distribute a claude-less roster");
        assert!(
            !base.join("claude").join("settings.json").exists(),
            "a claude-less roster writes no fence"
        );
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .all(|c| c.trust_flags.is_empty()));
        seen.lock().unwrap().clear();
        // A claude ballot ⇒ the shared fence exists with the shared rules, and the claude seat's
        // argv carries the state-home rules — the operational home included; codex is untouched.
        distribute_units_on(
            std::slice::from_ref(&unit),
            &[claude_seat("claude"), seat("codex")],
            "s1",
            None,
            &dispatcher,
            None,
            Some(&op_home),
        )
        .expect("distribute a roster with a claude ballot");
        let bytes = std::fs::read(base.join("claude").join("settings.json"))
            .expect("the shared fence was written before the ballot");
        let settings: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let deny: Vec<String> = settings["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny present")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        assert_eq!(
            deny,
            crate::execute_wrapped::shared_deny_rules(None).unwrap(),
            "the ballot reads the SAME shared fence a worker spawn writes"
        );
        let convened = seen.lock().unwrap().clone();
        let claude = convened
            .iter()
            .find(|c| c.key == "claude")
            .expect("the claude seat was convened");
        let codex = convened
            .iter()
            .find(|c| c.key == "codex")
            .expect("the codex seat was convened");
        assert!(
            codex.trust_flags.is_empty(),
            "a non-claude seat is handed back untouched"
        );
        let expected = crate::execute_wrapped::ballot_deny_rules(Some(&op_home)).unwrap();
        assert_eq!(
            claude.trust_flags,
            vec![
                "--dangerously-skip-permissions".to_string(),
                "--disallowedTools".to_string(),
                expected.join(","),
            ],
            "the claude ballot carries the state-home rules on its argv, after its own trust flag"
        );
        let opr = crate::execute_wrapped::rule_path(&op_home).unwrap();
        assert!(
            expected.contains(&format!("Read({opr}/**)"))
                && expected.contains(&format!("Edit({opr}/**)")),
            "{expected:?}"
        );
        match prev_hatch {
            Some(v) => std::env::set_var(hatch, v),
            None => std::env::remove_var(hatch),
        }
        std::env::set_var(
            wicked_apps_core::spawn::WORKER_HOME_ENV,
            wicked_apps_core::spawn::hermetic_test_worker_home(),
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_single_seat_roster_skips_the_council_and_dispatches_nothing() {
        // `distribute_units_on` resolves the skills ladder from the process environment when a
        // unit names a skill (none here) — held under the env READ lock regardless, so it can
        // never observe a variable another test is pinning under the write lock (#402 pass 3).
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let unit = WorkUnit::pending("u1", "s1", 0, "Write the parser module");

        // Single seat → short-circuit.
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SpyDispatcher {
            calls: calls.clone(),
        });
        let dists = distribute_units_on(
            std::slice::from_ref(&unit),
            &[seat("solo")],
            "s1",
            None,
            &dispatcher,
            None,
            None,
        )
        .expect("distribute a single-seat roster");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "a one-seat roster must NOT dispatch a ballot"
        );
        assert_eq!(dists.len(), 1);
        assert_eq!(dists[0].assigned_cli, "solo");
        assert_eq!(
            dists[0].assigned_invocation.as_deref(),
            Some("run-solo {PROMPT}")
        );
        assert!(
            dists[0].council_task_ref.is_none(),
            "no council task convened"
        );
        assert!(
            matches!(&dists[0].routing, RoutingInfo::Council { winner, agreement_pct, returned, seated, dissent }
                if winner == "solo" && *agreement_pct == 100 && *returned == 1 && *seated == Some(1) && *dissent == 0),
            "single seat records a truthful 1-of-1 verdict, got {:?}",
            dists[0].routing
        );
        assert!(
            dists[0].seat_constraint.is_none(),
            "a skill-free unit is unconstrained (core#401): {:?}",
            dists[0].seat_constraint
        );

        // Two seats → the council genuinely convenes (guard is scoped to len==1).
        let calls2 = Arc::new(AtomicUsize::new(0));
        let dispatcher2: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SpyDispatcher {
            calls: calls2.clone(),
        });
        let _ = distribute_units_on(
            &[unit],
            &[seat("alpha"), seat("beta")],
            "s1",
            None,
            &dispatcher2,
            None,
            None,
        )
        .expect("distribute a two-seat roster");
        assert!(
            calls2.load(Ordering::SeqCst) >= 1,
            "a multi-seat roster still convenes a council (dispatches ballots)"
        );
    }

    fn status_with_winner(winner: Option<&str>, state: TaskState) -> PollStatus {
        PollStatus {
            task_id: "t".into(),
            state,
            returned: 1,
            seated: 1,
            pending: 0,
            verdict: winner.map(|w| Verdict {
                task_id: "t".into(),
                kind: "Consensus".into(),
                consensus: true,
                seated: 1,
                winning_recommendation: Some(w.to_string()),
                agreement_ratio: 1.0,
                risk_convergence: vec![],
                dissent: vec![],
            }),
            seat_failures: vec![],
            failure_detail: None,
        }
    }

    fn cap_map(keys: &[&str]) -> Vec<(String, String)> {
        keys.iter()
            .map(|k| (format!("{k}-capabilities"), k.to_string()))
            .collect()
    }

    #[test]
    fn option_number_selects_correct_seat() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        // "2 — rationale" → index 1 → fake-b
        let st = status_with_winner(Some("2 — best fit for this task"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-b");
        assert!(
            matches!(&routing, RoutingInfo::Council { winner, agreement_pct, .. }
                if winner.as_str() == "fake-b" && *agreement_pct == 100),
            "option-2 winner maps to fake-b with Council provenance, got {routing:?}"
        );
    }

    #[test]
    fn bare_number_also_resolves() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let st = status_with_winner(Some("1"), TaskState::Voted);
        let (cli, _) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a");
    }

    #[test]
    fn no_status_degrades_to_first_seat() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let (cli, routing) = route_from_status(None, &roster, &map);
        assert_eq!(cli, "fake-a");
        assert!(matches!(routing, RoutingInfo::Degraded { .. }));
    }

    #[test]
    fn out_of_range_number_degrades() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        // 99 is out of range for a 2-option map
        let st = status_with_winner(Some("99 — some rationale"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a");
        assert!(
            matches!(&routing, RoutingInfo::Degraded { reason } if reason.contains("99")),
            "out-of-range option degrades with the recommendation in the reason, got {routing:?}"
        );
    }

    #[test]
    fn non_numeric_recommendation_degrades() {
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let st = status_with_winner(Some("Option Z"), TaskState::Voted);
        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a");
        assert!(
            matches!(&routing, RoutingInfo::Degraded { reason } if reason.contains("Option Z")),
            "non-numeric winner degrades with a reason naming the recommendation, got {routing:?}"
        );
    }

    #[test]
    fn a_council_that_failed_on_its_own_names_that_in_the_degrade_reason() {
        // A panic in synthesis, ranking or emission belongs to no seat, so `seat_failures` is
        // empty and the old text fell through to "(no per-seat reason recorded)" — exactly the
        // undiagnosable string FINDING-026 exists to eliminate, on the one path where the cause
        // WAS captured.
        let roster = vec!["fake-a".to_string(), "fake-b".to_string()];
        let map = cap_map(&["fake-a", "fake-b"]);
        let mut st = status_with_winner(None, TaskState::Failed);
        st.failure_detail = Some("rank store exploded".into());

        let (cli, routing) = route_from_status(Some(&st), &roster, &map);
        assert_eq!(cli, "fake-a", "the unit still routes somewhere");
        let RoutingInfo::Degraded { reason } = &routing else {
            panic!("a failed council degrades: {routing:?}");
        };
        assert!(
            reason.contains("rank store exploded"),
            "the council's own failure is the only account of what happened, got {reason:?}"
        );
        assert!(
            !reason.contains("no per-seat reason recorded"),
            "a recorded cause must not be reported as an absent one, got {reason:?}"
        );
    }

    /// RelaySink translates the council's string-keyed events into run-scoped CoreEvents
    /// with the owning (session, ord) attached, and drops non-run-scoped event types.
    #[test]
    fn relay_sink_translates_council_events_and_drops_the_rest() {
        use wicked_council::EventSink;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let relay: EventRelay = Arc::new(move |ev| sink_seen.lock().unwrap().push(ev));
        let sink = RelaySink {
            relay,
            session: "s1".to_string(),
            ord: 3,
        };

        sink.emit(
            wicked_apps_core::EV_COUNCIL_REQUESTED,
            &serde_json::json!({"clis": ["a", "b"], "task_id": "t", "session_id": "s1"}),
        );
        sink.emit(
            wicked_apps_core::EV_COUNCIL_VOTED,
            &serde_json::json!({"consensus": true, "agreement_ratio": 0.5, "votes": 4, "seated": 5}),
        );
        // Not run-scoped — must be dropped, not translated.
        sink.emit(wicked_apps_core::EV_CLI_RANKED, &serde_json::json!({}));

        let events = seen.lock().unwrap();
        assert_eq!(events.len(), 2, "ranked event dropped: {events:?}");
        assert!(
            matches!(&events[0], CoreEvent::CouncilConvened { session, ord, clis }
                if session == "s1" && *ord == 3 && clis == &["a".to_string(), "b".to_string()]),
            "requested → CouncilConvened with run scope, got {:?}",
            events[0]
        );
        assert!(
            matches!(&events[1], CoreEvent::CouncilVoted { session, ord, consensus: true, agreement_pct: 50, votes: 4, seated: Some(5) }
                if session == "s1" && *ord == 3),
            "voted → CouncilVoted with ratio as percent, got {:?}",
            events[1]
        );
    }

    /// Malformed payloads (missing/mistyped fields) degrade to defaults instead of panicking.
    #[test]
    fn relay_sink_survives_malformed_payloads() {
        use wicked_council::EventSink;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_seen = Arc::clone(&seen);
        let relay: EventRelay = Arc::new(move |ev| sink_seen.lock().unwrap().push(ev));
        let sink = RelaySink {
            relay,
            session: "s1".to_string(),
            ord: 1,
        };

        sink.emit(
            wicked_apps_core::EV_COUNCIL_REQUESTED,
            &serde_json::json!({}),
        );
        sink.emit(
            wicked_apps_core::EV_COUNCIL_VOTED,
            &serde_json::json!({"agreement_ratio": "not a number"}),
        );

        let events = seen.lock().unwrap();
        assert!(
            matches!(&events[0], CoreEvent::CouncilConvened { clis, .. } if clis.is_empty()),
            "missing clis → empty list, got {:?}",
            events[0]
        );
        assert!(
            matches!(
                &events[1],
                CoreEvent::CouncilVoted {
                    consensus: false,
                    agreement_pct: 0,
                    votes: 0,
                    // NOT `Some(0)` and NOT `Some(votes)`. A seat count nobody reported is
                    // unknown: `0` is an impossible denominator a consumer could divide by, and
                    // copying `votes` would assert that every seat answered — the false-complete
                    // reading this field exists to prevent (review on #151).
                    seated: None,
                    ..
                }
            ),
            "mistyped fields → zero defaults, absent seat count → unknown, got {:?}",
            events[1]
        );
    }

    // ── Seat selection honours skill portability (core#401) ────────────────────────────────

    use crate::domain::StageKind;
    use crate::skills_snapshot::test_support::{
        gen_dir, live_root, load, scratch, snapshot_root_with,
    };

    /// A roster seat whose record AND template both run `binary` — a claude seat when `binary` is
    /// `claude`, exactly as both runners would judge it at launch.
    fn seat_running(key: &str, binary: &str) -> AgenticCli {
        AgenticCli {
            binary: binary.into(),
            headless_invocation: format!("{binary} -p {{PROMPT}}"),
            ..seat(key)
        }
    }

    /// A unit at `ord` carrying `skill_ref`.
    fn skilled(ord: u32, skill_ref: &str) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("u{ord}"), "s1", ord, format!("Unit {ord}"));
        u.skill_ref = Some(skill_ref.to_string());
        u
    }

    /// A published snapshot holding a non-portable `wicked-garden-repo-learn` and a portable
    /// `wicked-garden-search` — the live shape (design v3.2 §3; 78 of garden's 142 skills are
    /// non-portable).
    fn published(name: &str) -> crate::skills_snapshot::SkillsSnapshot {
        let root = snapshot_root_with(
            &gen_dir(&scratch(name), "7"),
            "7",
            &[
                ("repo-learn", "wicked-garden-repo-learn", false, &[]),
                ("search", "wicked-garden-search", true, &[]),
            ],
        );
        load(&root)
    }

    /// The `(ord, roster keys)` of every `CouncilConvened` a relay saw.
    type Convened = Arc<std::sync::Mutex<Vec<(u32, Vec<String>)>>>;

    /// The relay + the run-scoped `CouncilConvened` events it saw, so a test can prove WHICH seats
    /// the council was convened over (the payload's `clis` is the roster it was given).
    fn convened_seats() -> (EventRelay, Convened) {
        let seen: Convened = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let relay: EventRelay = Arc::new(move |ev| {
            if let CoreEvent::CouncilConvened { ord, clis, .. } = ev {
                sink.lock().unwrap().push((ord, clis));
            }
        });
        (relay, seen)
    }

    fn spy() -> (Arc<dyn Dispatcher + Send + Sync>, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SpyDispatcher {
            calls: calls.clone(),
        });
        (dispatcher, calls)
    }

    /// Pins `HOME` to a directory for the test's lifetime (restored on drop). The merged council
    /// registry the carriers resolve seats from lives at `$HOME/.config/wicked-council/clis.toml`
    /// (`registry::default_user_path`), so every eligibility test reads a registry IT wrote — or
    /// the built-ins, when it wrote none — never the operator's.
    struct HomePin(Option<std::ffi::OsString>);

    impl HomePin {
        fn set(dir: &std::path::Path) -> Self {
            let prev = std::env::var_os("HOME");
            std::env::set_var("HOME", dir);
            Self(prev)
        }
    }

    impl Drop for HomePin {
        fn drop(&mut self) {
            match self.0.take() {
                Some(prev) => std::env::set_var("HOME", prev),
                None => std::env::remove_var("HOME"),
            }
        }
    }

    /// A hermetic registry home for one test: `HOME` pinned to a fresh canonical scratch dir under
    /// the crate's env write lock (like every other env-pinning test), and that dir. The pin is
    /// the FIRST element so it restores `HOME` before the lock is released.
    fn hermetic_home(
        name: &str,
    ) -> (
        HomePin,
        std::sync::RwLockWriteGuard<'static, ()>,
        std::path::PathBuf,
    ) {
        let env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let dir = scratch(name);
        let pin = HomePin::set(&dir);
        (pin, env, dir)
    }

    /// The operator's `clis.toml` under `home`, re-pointing the registry record for `key` at
    /// `binary` — a user record replaces its built-in WHOLESALE (`registry::load`), so this is
    /// what both carriers will resolve for that key from now on.
    fn override_seat(home: &std::path::Path, key: &str, binary: &str) {
        let council = home.join(".config").join("wicked-council");
        std::fs::create_dir_all(&council).unwrap();
        std::fs::write(
            council.join("clis.toml"),
            format!(
                "[[cli]]\nkey = \"{key}\"\ndisplay_name = \"{key} (override)\"\nbinary = \
                 \"{binary}\"\nheadless_invocation = \"{binary} run {{PROMPT}}\"\n"
            ),
        )
        .unwrap();
    }

    /// The live defect (core#401): a `capture-learnings` unit carrying `wicked-garden-repo-learn`
    /// (`portable: false`) was council-routed to copilot, which the ladder then refused by name.
    /// Now the unit's candidates are the claude seats — one here, so no ballot is dispatched and
    /// the unit lands on claude with a truthful 1-of-1 verdict and the constraint named. A unit
    /// whose skill IS portable is routed exactly as before: the council convenes over the WHOLE
    /// roster and no constraint is recorded. Mutation: drop the narrowing in `seat_candidates`
    /// and the first unit is voted onto copilot (option 1) with dispatches > 0.
    #[test]
    fn a_nonportable_skill_ref_is_seated_on_claude_while_a_portable_one_convenes_the_whole_roster()
    {
        let (_home, _env, _) = hermetic_home("route-nonportable-home");
        let snapshot = published("route-nonportable");
        let roster = [
            seat_running("copilot", "copilot"),
            seat_running("claude", "claude"),
            seat_running("pi", "pi"),
        ];

        // Non-portable ⇒ claude, no council over the others.
        let (dispatcher, calls) = spy();
        let (relay, convened) = convened_seats();
        let dists = distribute_units_against(
            &[skilled(1, "wicked-garden-repo-learn")],
            &roster,
            "s1",
            None,
            &dispatcher,
            Some(relay),
            Some(&snapshot),
            None,
        )
        .expect("a claude seat is on the roster: no refusal");
        assert_eq!(dists.len(), 1);
        assert_eq!(dists[0].assigned_cli, "claude", "{:?}", dists[0].routing);
        assert_eq!(
            dists[0].assigned_invocation.as_deref(),
            Some("claude -p {PROMPT}")
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "one eligible seat ⇒ nothing to elect, no ballot dispatched"
        );
        assert!(
            convened.lock().unwrap().is_empty(),
            "no council convened over a single candidate"
        );
        assert!(
            matches!(&dists[0].routing, RoutingInfo::Council { winner, returned: 1, seated: Some(1), .. }
                if winner == "claude"),
            "the routing method reads as today (a truthful 1-of-1 verdict), got {:?}",
            dists[0].routing
        );
        let why = dists[0]
            .seat_constraint
            .as_deref()
            .expect("the narrowing is recorded");
        assert!(
            why.contains("wicked-garden-repo-learn") && why.contains("portable: false"),
            "the constraint names the skill and its portability: {why}"
        );
        assert!(
            why.contains("gen=7"),
            "…and the generation it was read from: {why}"
        );

        // Portable ⇒ unchanged: the council convenes over the whole roster, no constraint.
        let (dispatcher, calls) = spy();
        let (relay, convened) = convened_seats();
        let dists = distribute_units_against(
            &[skilled(2, "wicked-garden-search")],
            &roster,
            "s1",
            None,
            &dispatcher,
            Some(relay),
            Some(&snapshot),
            None,
        )
        .expect("portable skill: routed as before");
        assert!(
            dists[0].seat_constraint.is_none(),
            "{:?}",
            dists[0].seat_constraint
        );
        assert!(
            calls.load(Ordering::SeqCst) >= 1,
            "the council genuinely convened"
        );
        assert_eq!(
            *convened.lock().unwrap(),
            vec![(2, vec!["copilot".to_string(), "claude".into(), "pi".into()])],
            "…over every roster seat"
        );
        assert_eq!(
            dists[0].assigned_cli, "copilot",
            "the spy votes option 1 of the FULL roster"
        );
    }

    /// A Claude-less roster cannot seat a non-portable skill anywhere, so the run is refused at
    /// DISTRIBUTION — plan-wide, before any unit ran (the skill-free first unit included), with no
    /// council convened — naming the skill, its portability, the seat kind required and the roster
    /// that lacks it. Before: the council seated it, the ladder refused it mid-run, and the
    /// escalation gate could only re-dispatch to the same seat or cancel.
    #[test]
    fn a_claude_less_roster_is_refused_at_plan_time_naming_skill_portability_and_seat_kind() {
        let (_home, _env, _) = hermetic_home("route-refuse-home");
        let snapshot = published("route-refuse");
        let roster = [seat_running("copilot", "copilot"), seat_running("pi", "pi")];
        let mut first = WorkUnit::pending("u1", "s1", 1, "Recon: read the repo");
        first.skill_ref = None;
        let (dispatcher, calls) = spy();
        let err = distribute_units_against(
            &[first, skilled(2, "wicked-garden-repo-learn")],
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            Some(&snapshot),
            None,
        )
        .expect_err("no claude seat on the roster");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "refused before ANY council convened — the skill-free first unit included"
        );
        let refusal = err
            .downcast_ref::<SkillsError>()
            .unwrap_or_else(|| panic!("a skills refusal, got {err:?}"));
        assert!(
            matches!(refusal, SkillsError::NoEligibleSeat { ord: 2, required_seat: "claude", skills, roster, .. }
                if skills == &["wicked-garden-repo-learn".to_string()]
                    && roster == &["copilot".to_string(), "pi".into()]),
            "{refusal:?}"
        );
        let text = err.to_string();
        for needle in [
            "unit 2 requires wicked-garden-repo-learn",
            "only a claude seat can be handed",
            "portable: false",
            "roster [copilot, pi] holds no seat that resolves to claude",
            "before any unit ran",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in: {text}");
        }
    }

    /// The live-cache FALLBACK (no snapshot published) is Claude-only for ANY skill it holds —
    /// what the ladder already enforces at launch (`FallbackClaudeOnly`, #396 codex round 7) — so
    /// the routing seats a skill-bearing unit on claude before the council can pick otherwise,
    /// and a Claude-less roster is refused saying so.
    #[test]
    fn the_live_cache_fallback_seats_every_skill_bearing_unit_on_claude() {
        let (_home, _env, _) = hermetic_home("route-live-home");
        let config = scratch("route-live").join("claude-config");
        live_root(
            &config
                .join("plugins")
                .join("cache")
                .join("wicked-garden")
                .join("wicked-garden")
                .join("1.0.0"),
            "1.0.0",
            &[("search", "wicked-garden-search")],
        );
        let snapshot = crate::skills_snapshot::resolve_in(None, Some(config), None, &mut |_| {})
            .unwrap()
            .expect("the live cache is a root");
        assert_eq!(
            snapshot.source,
            crate::skills_snapshot::SnapshotSource::LiveCache
        );
        // The same skill is PORTABLE by the text approximation — irrelevant: nobody published it.
        assert!(snapshot.skill("wicked-garden-search").unwrap().portable);

        let (dispatcher, calls) = spy();
        let dists = distribute_units_against(
            &[skilled(1, "wicked-garden-search")],
            &[seat_running("pi", "pi"), seat_running("claude", "claude")],
            "s1",
            None,
            &dispatcher,
            None,
            Some(&snapshot),
            None,
        )
        .expect("a claude seat is on the roster");
        assert_eq!(dists[0].assigned_cli, "claude");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let why = dists[0].seat_constraint.as_deref().expect("constrained");
        assert!(
            why.contains("live plugin cache") && why.contains("wicked-garden-search"),
            "{why}"
        );

        let err = distribute_units_against(
            &[skilled(1, "wicked-garden-search")],
            &[seat_running("pi", "pi")],
            "s1",
            None,
            &dispatcher,
            None,
            Some(&snapshot),
            None,
        )
        .expect_err("Claude-less roster under the fallback");
        assert!(
            err.to_string().contains("live plugin cache")
                && err
                    .to_string()
                    .contains("holds no seat that resolves to claude"),
            "{err}"
        );
    }

    /// No root to judge against (`None`: the ladder was absent, failed or misconfigured) ⇒ no
    /// seat is constrained and nothing is refused here — the launch admission decides at the first
    /// unit, exactly as before this change. A tool unit is never constrained either.
    #[test]
    fn without_a_root_or_for_a_tool_unit_nothing_is_constrained() {
        let (dispatcher, _) = spy();
        let mut tool = skilled(2, "wicked-garden-repo-learn");
        tool.tool_cmd = Some(vec!["echo".into(), "hi".into()]);
        let dists = distribute_units_against(
            &[skilled(1, "wicked-garden-repo-learn"), tool],
            &[seat_running("pi", "pi"), seat_running("claude", "claude")],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
        )
        .expect("no root ⇒ no routing-time refusal");
        assert!(
            dists.iter().all(|d| d.seat_constraint.is_none()),
            "{dists:?}"
        );
        assert_eq!(
            dists[0].assigned_cli, "pi",
            "the spy's option 1 of the whole roster"
        );
        assert!(matches!(dists[1].routing, RoutingInfo::Tool));

        // A tool unit is skipped even WITH a root that would constrain an agent unit.
        let snapshot = published("route-tool");
        let mut tool = skilled(1, "wicked-garden-repo-learn");
        tool.tool_cmd = Some(vec!["echo".into()]);
        let dists = distribute_units_against(
            &[tool],
            &[seat_running("pi", "pi")],
            "s1",
            None,
            &dispatcher,
            None,
            Some(&snapshot),
            None,
        )
        .expect("a tool unit has no seat to constrain");
        assert!(dists[0].seat_constraint.is_none());
    }

    /// The eligible claude seats are judged ONCE per pass (Copilot, #402 review pass 4): a plan
    /// with several Claude-only units resolves each roster seat exactly once — one merged-registry
    /// read per seat, not per seat per unit — and every such unit receives the identical roster.
    /// Mutation: recompute inside the per-unit closure and the count becomes units × seats.
    #[test]
    fn several_claude_only_units_judge_each_roster_seat_once_and_see_one_roster() {
        let (_home, _env, _) = hermetic_home("route-once-home");
        let snapshot = published("route-once");
        let roster = [seat_running("claude", "claude"), seat_running("pi", "pi")];
        let units: Vec<WorkUnit> = (1..=4)
            .map(|ord| skilled(ord, "wicked-garden-repo-learn"))
            .collect();
        let before = super::SEAT_JUDGEMENTS.with(|n| n.get());
        let candidates = seat_candidates(&units, &roster, Some(&snapshot)).expect("candidates");
        let judged = super::SEAT_JUDGEMENTS.with(|n| n.get()) - before;
        assert_eq!(
            judged,
            roster.len(),
            "each roster seat is judged once per pass, not {} units × {} seats",
            units.len(),
            roster.len()
        );
        let rosters: Vec<Vec<String>> = candidates
            .iter()
            .map(|c| {
                let (eligible, _why) = c.as_ref().expect("a Claude-only unit is constrained");
                eligible.iter().map(|s| s.key.clone()).collect()
            })
            .collect();
        assert_eq!(rosters.len(), units.len());
        assert!(rosters.iter().all(|r| r == &rosters[0]), "{rosters:?}");
        assert_eq!(rosters[0], vec!["claude".to_string()]);
    }

    /// Evaluator ≠ creator must not undo the narrowing: a Claude-only REVIEW unit whose council
    /// pick is the builder's seat is NOT moved onto a seat that cannot take it (it stays put,
    /// exactly as when the roster has no alternative at all), while an unconstrained review unit
    /// is still moved off the builder as before. Mutation: drop the `admits` filter and the
    /// constrained review unit lands on pi — the refusal this change exists to prevent.
    #[test]
    fn evaluator_distinct_never_moves_a_claude_only_review_unit_onto_a_seat_that_cannot_take_it() {
        let (_home, _env, _) = hermetic_home("route-evaluator-home");
        let snapshot = published("route-evaluator");
        let roster = [seat_running("claude", "claude"), seat_running("pi", "pi")];
        let mut build = WorkUnit::pending("u1", "s1", 1, "Build the thing");
        build.stage = StageKind::Build;
        let mut review_nonportable = skilled(2, "wicked-garden-repo-learn");
        review_nonportable.stage = StageKind::Review;
        let mut review_portable = skilled(3, "wicked-garden-search");
        review_portable.stage = StageKind::Review;
        let (dispatcher, _) = spy();
        let dists = distribute_units_against(
            &[build, review_nonportable, review_portable],
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            Some(&snapshot),
            None,
        )
        .expect("distributed");
        // The spy votes option 1 everywhere: the builder is claude.
        assert_eq!(dists[0].assigned_cli, "claude");
        // Claude-only review: claude is the builder, pi cannot take the skill ⇒ stays on claude,
        // routing untouched (no EvaluatorDistinct claim for a move that did not happen).
        assert_eq!(dists[1].assigned_cli, "claude");
        assert!(
            matches!(dists[1].routing, RoutingInfo::Council { .. }),
            "{:?}",
            dists[1].routing
        );
        assert!(dists[1].seat_constraint.is_some());
        // Unconstrained review: moved off the builder onto pi, as before.
        assert_eq!(dists[2].assigned_cli, "pi");
        assert!(
            matches!(&dists[2].routing, RoutingInfo::EvaluatorDistinct { winner, was }
                if winner == "pi" && was == "claude"),
            "{:?}",
            dists[2].routing
        );
        assert!(dists[2].seat_constraint.is_none());
    }

    /// #402 review pass 2: eligibility is judged by the SAME resolutions the carriers execute —
    /// the ACP carrier's merged registry record by key (`acp_seat_identity`, the very function
    /// `exec_turn_inner` calls) and the wrapped carrier's launch template
    /// (`wrapped_seat_identity`, composed of the two resolutions `exec` makes) — and NEVER by the
    /// roster record's own fields, so a custom or overridden record cannot pass routing as claude
    /// and execute as something else (the #401 mid-run refusal, recreated). Hermetic: `HOME` is
    /// pinned, so the registry is the built-ins plus whatever `clis.toml` THIS test writes.
    /// Mutation: judge the roster record's `binary` instead and the override case below seats the
    /// unit on a seat the ACP carrier would refuse.
    #[test]
    fn eligibility_follows_the_carriers_seat_resolution_not_the_roster_record() {
        use crate::acp_runner::acp_seat_identity;
        use crate::execute_wrapped::wrapped_seat_identity;
        use crate::skills_snapshot::WorkerCli;
        let (_home, _env, home) = hermetic_home("route-override");
        let snapshot = published("route-override");
        let (dispatcher, _) = spy();

        // Built-in registry: `claude` resolves to claude on both carriers ⇒ eligible, and the ACP
        // runner's own judgement agrees. A quoted template path with spaces is one token.
        let claude = seat_running("claude", "claude");
        assert!(seat_is_claude(std::slice::from_ref(&claude), "claude"));
        assert!(matches!(acp_seat_identity("claude"), WorkerCli::Claude));
        assert!(seat_is_claude(
            &[AgenticCli {
                headless_invocation: r#""/Applications/Claude Tools/claude" -p {PROMPT}"#.into(),
                ..seat("claude")
            }],
            "claude"
        ));
        assert!(!seat_is_claude(&[seat_running("pi", "pi")], "pi"));

        // A roster record keyed `claude` whose TEMPLATE runs codex: the wrapped carrier would run
        // codex ⇒ not eligible, although the registry (the ACP carrier) says claude. The record's
        // own `binary` saying `claude` changes nothing — no runner reads it.
        let codex_template = AgenticCli {
            binary: "claude".into(),
            headless_invocation: "codex exec {PROMPT}".into(),
            ..seat("claude")
        };
        assert!(matches!(
            wrapped_seat_identity(
                "claude",
                invocation_of(std::slice::from_ref(&codex_template), "claude")
            ),
            WorkerCli::Other { .. }
        ));
        assert!(!seat_is_claude(
            std::slice::from_ref(&codex_template),
            "claude"
        ));

        // An unregistered key whose template is claude: the ACP carrier judges the key as its own
        // binary ⇒ the carriers disagree ⇒ not eligible (refused at plan time, never seated).
        assert!(!seat_is_claude(
            &[seat_running("my-claude", "claude")],
            "my-claude"
        ));

        // THE OVERRIDE, end to end: the operator's clis.toml re-points `claude` at a codex
        // carrier. The ACP runner's resolution says codex; so does routing; and a Claude-only
        // unit on [claude, pi] — a roster whose `claude` record LOOKS like claude — is refused at
        // plan time, before any council convenes, instead of seated and refused mid-run.
        override_seat(&home, "claude", "codex");
        assert!(
            matches!(acp_seat_identity("claude"), WorkerCli::Other { ref key, .. } if key == "claude"),
            "the ACP carrier would execute the override"
        );
        assert!(!seat_is_claude(std::slice::from_ref(&claude), "claude"));
        let err = distribute_units_against(
            &[skilled(1, "wicked-garden-repo-learn")],
            &[claude.clone(), seat_running("pi", "pi")],
            "s1",
            None,
            &dispatcher,
            None,
            Some(&snapshot),
            None,
        )
        .expect_err("no seat resolves to claude on both carriers");
        assert!(
            matches!(err.downcast_ref::<SkillsError>(), Some(SkillsError::NoEligibleSeat { roster, .. })
                if roster == &["claude".to_string(), "pi".into()]),
            "{err:?}"
        );
        assert!(err.to_string().contains("not the key's spelling"), "{err}");

        // …and vice versa: the override re-points `codex` at claude. A roster seat keyed `codex`
        // with an EMPTY template (so the wrapped carrier resolves the registry's) resolves to
        // claude on both carriers ⇒ eligible ⇒ the unit is seated on it, no refusal.
        override_seat(&home, "codex", "claude");
        assert!(matches!(acp_seat_identity("codex"), WorkerCli::Claude));
        let codex_key = AgenticCli {
            headless_invocation: String::new(),
            ..seat("codex")
        };
        assert!(seat_is_claude(std::slice::from_ref(&codex_key), "codex"));
        let dists = distribute_units_against(
            &[skilled(1, "wicked-garden-repo-learn")],
            &[seat_running("pi", "pi"), codex_key],
            "s1",
            None,
            &dispatcher,
            None,
            Some(&snapshot),
            None,
        )
        .expect("the overridden `codex` seat IS a claude seat");
        assert_eq!(dists[0].assigned_cli, "codex");
        assert!(dists[0].seat_constraint.is_some());
        // Its launch resolves the registry template — no roster template was carried.
        assert_eq!(dists[0].assigned_invocation, None);
    }

    /// F-7R2-006 (wave 6): a seat the launcher's health probe found unusable is BENCHED — never
    /// handed a ballot, absent from `councilConvened.clis`, never the winner — and
    /// `degradedReason` names it on the council arm, where it used to be `null`.
    #[test]
    fn a_launcher_benched_seat_is_never_convened_and_degraded_reason_names_it() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> =
            Arc::new(RecordingDispatcher { seen: seen.clone() });
        let (relay, convened) = convened_seats();
        let mut signed_out = seat("codex");
        signed_out.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        let roster = [seat("claude"), signed_out, seat("pi")];
        let unit = WorkUnit::pending("u1", "s1", 1, "Build the thing");
        let dists = distribute_units_against_benched(
            &[unit],
            &roster,
            "s1",
            None,
            &dispatcher,
            Some(relay),
            None,
            None,
            &[],
        )
        .expect("two eligible seats route");
        assert!(
            seen.lock().unwrap().iter().all(|c| c.key != "codex"),
            "a benched seat is never handed a ballot"
        );
        let convened = convened.lock().unwrap();
        assert!(
            !convened.is_empty(),
            "a council convened over the eligible seats"
        );
        for (_, clis) in convened.iter() {
            assert_eq!(
                clis,
                &vec!["claude".to_string(), "pi".to_string()],
                "councilConvened.clis names only eligible seats"
            );
        }
        assert_ne!(dists[0].assigned_cli, "codex");
        assert!(matches!(dists[0].routing, RoutingInfo::Council { .. }));
        assert_eq!(
            dists[0].degraded_reason.as_deref(),
            Some("1 of 3 seats benched: codex (signed out — launcher)")
        );
        assert_eq!(dists[0].benched.len(), 1);
        assert_eq!(dists[0].benched[0].source, "launcher");
    }

    /// Every configured seat benched ⇒ the plan is REFUSED by name, before any council.
    #[test]
    fn an_all_benched_roster_is_refused_by_name() {
        let (dispatcher, calls) = spy();
        let mut a = seat("codex");
        a.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        let mut b = seat("pi");
        b.health = Some(wicked_council::types::SeatHealth::unusable(
            "dispatch budget",
        ));
        let err = distribute_units_against_benched(
            &[WorkUnit::pending("u1", "s1", 1, "Build")],
            &[a, b],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect_err("no eligible seat");
        let msg = err.to_string();
        assert!(msg.contains("every configured seat is benched"), "{msg}");
        assert!(
            msg.contains("codex (signed out — launcher)")
                && msg.contains("pi (dispatch budget — launcher)"),
            "{msg}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no ballot dispatched");
    }

    // ── F-E2E-011: the seat requirement is per UNIT — a tool-only plan needs no seat ──────────

    /// A Tool-executor unit in the seeded onboarding shape (`wicked-estate <verb> …`).
    fn tool_unit(ord: u32, verb: &str) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("u{ord}"), "s1", ord, format!("{verb} the repo"));
        u.tool_cmd = Some(vec!["wicked-estate".into(), verb.into()]);
        u
    }

    /// crew hands a tool-only workflow (`onboarding`) `clis: []` (wicked-crew#533): every unit
    /// routes `tool` to the tool's own program, no ballot is dispatched and nothing is refused —
    /// core-ts 0.7.22 bailed "every configured seat is benched" here, 1 s into every onboarding.
    #[test]
    fn a_tool_only_plan_needs_no_seat_and_routes_tool_on_an_empty_roster() {
        let (dispatcher, calls) = spy();
        let dists = distribute_units_against_benched(
            &[tool_unit(1, "index"), tool_unit(2, "clusters")],
            &[],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect("a plan that seats nobody is not refused for having no seat");
        assert_eq!(dists.len(), 2);
        for d in &dists {
            assert_eq!(d.assigned_cli, "wicked-estate");
            assert!(matches!(d.routing, RoutingInfo::Tool), "{:?}", d.routing);
            assert_eq!(d.assigned_invocation, None);
            assert_eq!(d.council_task_ref, None);
            assert_eq!(d.degraded_reason, None);
            assert!(d.benched.is_empty());
        }
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no ballot dispatched");
    }

    /// A tool-only plan on a roster whose EVERY seat is benched proceeds too — the seats are
    /// irrelevant to it — and the whole bench (prior + launcher) rides each distribution for the
    /// actor to persist, exactly as it would for a seated plan.
    #[test]
    fn a_tool_only_plan_on_an_all_benched_roster_routes_tool_and_carries_the_bench() {
        let (dispatcher, calls) = spy();
        let mut codex = seat("codex");
        codex.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        let prior = BenchedSeat {
            cli: "pi".into(),
            reason: "not_logged_in".into(),
            source: "ballot".into(),
        };
        let dists = distribute_units_against_benched(
            &[tool_unit(1, "index")],
            &[codex, seat("pi")],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[prior],
        )
        .expect("the tool unit routes; the bench is not a refusal");
        assert_eq!(dists[0].assigned_cli, "wicked-estate");
        assert!(matches!(dists[0].routing, RoutingInfo::Tool));
        assert_eq!(
            dists[0].degraded_reason, None,
            "a tool unit is never degraded by the bench"
        );
        let mut benched: Vec<(&str, &str)> = dists[0]
            .benched
            .iter()
            .map(|b| (b.cli.as_str(), b.source.as_str()))
            .collect();
        benched.sort();
        assert_eq!(benched, vec![("codex", "launcher"), ("pi", "ballot")]);
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no ballot dispatched");
    }

    /// The refusal is unchanged the moment a unit NEEDS a seat: a tool unit beside an agent unit
    /// on an empty roster is refused by the existing message, before any ballot.
    #[test]
    fn a_mixed_plan_with_an_agent_unit_and_no_seat_is_still_refused() {
        let (dispatcher, calls) = spy();
        let err = distribute_units_against_benched(
            &[
                tool_unit(1, "index"),
                WorkUnit::pending("u2", "s1", 2, "Build the thing"),
            ],
            &[],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect_err("the agent unit needs a seat and none is configured");
        let msg = err.to_string();
        assert!(
            msg.contains("no eligible seat for s1")
                && msg.contains("every configured seat is benched"),
            "{msg}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no ballot dispatched");
    }

    /// (F-7R2-006 unchanged) …and on a roster whose every seat is benched, the refusal still
    /// names the benched seats — the tool unit does not lend the agent unit a seat.
    #[test]
    fn a_mixed_plan_on_an_all_benched_roster_is_still_refused_by_name() {
        let (dispatcher, calls) = spy();
        let mut codex = seat("codex");
        codex.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        let err = distribute_units_against_benched(
            &[
                tool_unit(1, "index"),
                WorkUnit::pending("u2", "s1", 2, "Build the thing"),
            ],
            &[codex],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect_err("no eligible seat for the agent unit");
        let msg = err.to_string();
        assert!(
            msg.contains("every configured seat is benched")
                && msg.contains("codex (signed out — launcher)"),
            "{msg}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no ballot dispatched");
    }

    /// A seat whose BALLOT fails authentication is benched the moment the councils return: the
    /// unit the council handed it is reassigned to the first still-eligible seat (routing
    /// `degraded`, naming both seats), and the bench rides every distribution.
    #[test]
    fn a_ballot_that_fails_authentication_benches_the_seat_and_reassigns_its_unit() {
        use wicked_council::types::{BallotContext, DispatchOutcome, SeatFailure, SeatFailureKind};
        /// The first seat (`codex`) exits "Not logged in"; every other seat votes for option 1,
        /// which IS codex — the council's pick is a seat that cannot take the work.
        struct SignedOutCodex;
        impl Dispatcher for SignedOutCodex {
            fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
                Some(Vote {
                    cli: cli.key.clone(),
                    recommendation: "1 — fit".into(),
                    top_risk: "none".into(),
                    change_my_mind: "no".into(),
                    disqualifier: None,
                    confidence: Confidence::default(),
                    provenance: "test".into(),
                })
            }
            fn dispatch_ballot_detailed(
                &self,
                cli: &AgenticCli,
                task: &CouncilTask,
                _ctx: &BallotContext,
            ) -> DispatchOutcome {
                if cli.key == "codex" {
                    DispatchOutcome::Failed(
                        SeatFailure::new(SeatFailureKind::NonZeroExit, "exit 1")
                            .with_output("Not logged in · Please run /login", ""),
                    )
                } else {
                    DispatchOutcome::Voted(self.dispatch(cli, task).unwrap())
                }
            }
        }
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SignedOutCodex);
        let roster = [seat("codex"), seat("claude"), seat("pi")];
        let dists = distribute_units_against_benched(
            &[WorkUnit::pending("u1", "s1", 1, "Build the thing")],
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect("two seats still eligible");
        assert_eq!(
            dists[0].assigned_cli, "claude",
            "reassigned to the first still-eligible seat: {:?}",
            dists[0].routing
        );
        match &dists[0].routing {
            RoutingInfo::Degraded { reason } => assert!(
                reason.contains("council picked 'codex'")
                    && reason.contains("failed authentication on its ballot")
                    && reason.contains("reassigned to 'claude'"),
                "{reason}"
            ),
            other => panic!("expected a degraded routing, got {other:?}"),
        }
        let why = dists[0].degraded_reason.as_deref().unwrap();
        assert!(
            why.contains("1 of 3 seats benched: codex (not_logged_in — ballot)"),
            "{why}"
        );
        assert_eq!(dists[0].benched[0].cli, "codex");
        assert_eq!(dists[0].benched[0].source, "ballot");
    }

    /// The evaluator≠creator reassignment picks only among still-eligible seats — a benched
    /// seat is skipped even when it is the first non-builder on the roster.
    #[test]
    fn evaluator_distinct_never_moves_a_review_unit_onto_a_benched_seat() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher { seen });
        let mut benched = seat("b");
        benched.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        let roster = [seat("a"), benched, seat("c")];
        let build = WorkUnit::pending("u1", "s1", 1, "Build the thing");
        let mut review = WorkUnit::pending("u2", "s1", 2, "Review the thing");
        review.stage = crate::domain::StageKind::Review;
        let dists = distribute_units_against_benched(
            &[build, review],
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect("routes");
        assert_eq!(dists[0].assigned_cli, "a");
        assert_eq!(
            dists[1].assigned_cli, "c",
            "the review moves past the benched seat: {:?}",
            dists[1].routing
        );
        assert!(matches!(
            &dists[1].routing,
            RoutingInfo::EvaluatorDistinct { winner, was } if winner == "c" && was == "a"
        ));
        assert!(
            dists[1]
                .degraded_reason
                .as_deref()
                .is_some_and(|w| w.contains("b (signed out — launcher)")),
            "degradedReason rides the evaluator_distinct arm too: {:?}",
            dists[1].degraded_reason
        );
    }
}
