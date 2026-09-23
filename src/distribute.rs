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
    /// (core#461, core#591) The evaluator≠creator DISCLOSURE, first-class. Two values:
    ///
    /// * `Some("creator_seat")` — this is a review/test unit that STAYS on a seat that built what
    ///   it checks because no eligible seat distinct from the builders admits it.
    /// * `Some("same_cli_instance")` (core#591) — this review/test unit IS on a seat distinct from
    ///   every builder seat, but that seat runs the SAME CLI as a builder (`claude#2` grading
    ///   `claude#1`). A second instance removes context contamination — the evaluator holds none
    ///   of the creator's authoring reasoning — but NOT model-level blind spots: same weights,
    ///   same failure modes. Reading it as a model-distinct evaluator is the silent degradation
    ///   this field exists to prevent, so it is disclosed rather than assumed away.
    ///
    /// `creator_seat` DOMINATES: a unit that stays on a builder seat is disclosed as that, not as
    /// an instance fallback.
    ///
    /// The `creator_seat` paragraphs below describe that value only.
    ///
    /// The roster is necessarily BENCH-FREE when this is set (core#560/#567): a bench that leaves
    /// a review/test unit no distinct seat refuses the plan instead (`NoEligibleSeat`), so it never
    /// reaches this field. Three bench-free shapes reach it — a one-seat roster; a roster of two or
    /// more seats where EVERY seat was assigned a Build/Recon unit; and a roster that does have a
    /// non-builder seat which this unit's skills refuse (`seat_candidates`). `degraded_reason` does
    /// NOT name this — it is disclosed by this field alone.
    ///
    /// The seat the unit stays on is therefore always a still-ELIGIBLE one, and by construction
    /// rather than by any reseating: `launcher_benched` is folded into `benched` before anything
    /// else runs, so an empty bench means no seat was benched by a ballot, a worker OR the
    /// launcher's health probe — which is exactly what `eligible_seats` filters on. (Before
    /// core#560 this field could be set WITH a bench, and the guarantee rested on the bench pass
    /// having reseated the unit first; that path is now refused, so the reseat pass can no longer
    /// be what makes this true.)
    ///
    /// `None` when the unit is separated, is not an evaluator, or is a tool unit. Rides
    /// `unitDistributed.distinctnessFallback`.
    pub distinctness_fallback: Option<String>,
}

/// (core#461) [`Distribution::distinctness_fallback`] when the evaluator STAYS on a creator seat.
pub(crate) const DISTINCTNESS_FALLBACK_CREATOR_SEAT: &str = "creator_seat";

/// (core#591) [`Distribution::distinctness_fallback`] when the evaluator is a DIFFERENT seat
/// instance that runs the SAME CLI as a creator — instance-distinct, not model-distinct.
pub(crate) const DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE: &str = "same_cli_instance";

/// The CLI (MODEL) key a roster seat key names (core#591): `claude#2` and `claude` are two seat
/// INSTANCES of one CLI. Spelled once, over [`wicked_apps_core::spawn::seat_cli_key`] — the same
/// split the configuration-home resolver keys a seat's root on — so routing and isolation cannot
/// disagree about which seats share a model.
fn model_of(key: &str) -> &str {
    wicked_apps_core::spawn::seat_cli_key(key)
}

/// The invocation template for `key` from the launch roster (`None` if not found).
///
/// `key` is a seat INSTANCE key, which is unique across the roster: `refuse_duplicate_seat_keys`
/// rejects the plan before any distribution runs, so this `find` is unambiguous by construction
/// rather than by luck. Before core#591 a duplicate key silently resolved to the FIRST record and
/// the second instance's own template was unreachable.
pub(crate) fn invocation_of(clis: &[AgenticCli], key: &str) -> Option<String> {
    clis.iter()
        .find(|c| c.key == key)
        .map(|c| c.headless_invocation.clone())
        .filter(|s| !s.trim().is_empty())
}

/// (core#591) Refuse a launch roster that names one seat key twice, BEFORE anything routes.
///
/// A seat key is an INSTANCE identity: it is what `assigned_cli` carries, what
/// [`invocation_of`] resolves a launch template through, what the evaluator≠creator fence
/// compares seats by, and what the configuration-home resolver keys a seat's root on. Two records
/// sharing one key make all four ambiguous at once and NONE of them say so — the template resolves
/// to whichever record came first, and the fence reads the two records as one seat and leaves a
/// review unit on its creator. A second instance of one CLI is spelled
/// `claude{SEAT_INSTANCE_SEP}2`, which is a different key.
fn refuse_duplicate_seat_keys(configured: &[AgenticCli]) -> anyhow::Result<()> {
    use wicked_apps_core::spawn::SEAT_INSTANCE_SEP;
    let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for c in configured {
        if !seen.insert(c.key.as_str()) {
            anyhow::bail!(
                "launch roster names the seat key '{key}' more than once. A key is a seat \
                 INSTANCE identity (core#591): duplicates make the launch template, the \
                 evaluator\u{2260}creator fence and the seat's configuration home all resolve to \
                 whichever record came first. Spell a second instance of one cli as \
                 '{key}{SEAT_INSTANCE_SEP}2'.",
                key = c.key
            );
        }
    }
    Ok(())
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
        distinctness_fallback: None,
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
/// SEAT IDENTITY, under every rule below (core#591): a roster seat key is an INSTANCE identity —
/// `claude` and `claude#2` are two seats running one cli — and it must be unique across the
/// roster, which is refused at entry if it is not (`refuse_duplicate_seat_keys`). Routing,
/// `assigned_cli` and the wire carry the INSTANCE; a predicate about the MODEL reads the cli key
/// behind it (`model_of`): `seat_is_claude`, and the `same_cli_instance` disclosure in rule 2.
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
/// 2. BALLOT LEDGER — every seat's outcome on every council this distribution convened is
///    tallied ([`SeatLedger`]); a seat the ledger finds DEAD for the run is benched the moment the
///    councils return (F-7R2-006 / F-7R3-001): a `not_logged_in` or `not_installed` ballot on
///    first occurrence; NO vote on any ballot with every failure quota-class
///    (`quota_exhausted`), or a timeout streak of at least the dispatcher's own bench threshold
///    (`timed_out`). One vote keeps a seat; an unclassified failure proves nothing; the
///    dispatcher's own `Benched` abstention corroborates a dead-class ballot beside it and proves
///    nothing alone (core#461 — the ledger sees EVERY round of every council, not the latest). A
///    unit the council handed to a benched seat is reassigned to the first still-eligible seat its
///    skills admit (routing `degraded`, naming both seats and the cause), and the evaluator≠creator
///    reassignment picks only among still-eligible seats. When a BENCH leaves a review/test unit no
///    seat distinct from its creator the plan is REFUSED (`NoEligibleSeat`, core#560) — for every
///    bench source, not just ballot. A BENCH-FREE roster with no seat distinct from the builders
///    instead keeps the review/test unit on its creator seat: a one-seat roster, a roster whose
///    every seat was assigned a Build/Recon unit, or one whose only non-builder seats this unit's
///    skills refuse. That case is disclosed by the `distinctness_fallback: "creator_seat"` field
///    (core#461), not by `degraded_reason`; only the all-seats-built shape ALSO warns on stderr
///    (a one-seat roster never does — it had nothing to separate). A review/test unit that DID get
///    a seat distinct from every builder seat, but one running the same CLI (`claude#2` grading
///    `claude#1` — two seat INSTANCES of one cli, core#591), is disclosed by the same field as
///    `"same_cli_instance"`: instance-distinct, not model-distinct.
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
    // (core#591) BEFORE anything reads the roster: a duplicate seat key makes the launch template,
    // the evaluator≠creator fence and the seat's configuration home all ambiguous at once, and
    // none of the three says so. Refused, not disambiguated — a plan whose seat identities are
    // ambiguous is not a plan. Runs even for an all-tool plan: the roster is wrong either way.
    refuse_duplicate_seat_keys(configured)?;
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
    if eligible.is_empty() && configured.is_empty() {
        // An EMPTY roster with a unit that needs a seat is a configuration error, not a bench:
        // there is no seat to sign in, so the run fails as before (F-E2E-011: crew hands a
        // tool-only def `clis: []` by design; an agent unit on that roster is crew's bug).
        anyhow::bail!(
            "no eligible seat for {session_id}: every configured seat is benched — {} (sign a \
             seat in, or add one; a council over dead seats would only park the run at a human \
             gate per unit)",
            crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
        );
    }
    if eligible.is_empty() {
        // (D-10) The SAME typed refusal the intake uses, with the bench as data: the actor's
        // `PlanFailed` arm parks the run at the cursor's `dead_seat` gate instead of failing it.
        return Err(crate::NoEligibleSeat {
            run_id: session_id.to_string(),
            benched: crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default(),
            benched_seats: benched.clone(),
        }
        .into());
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
        // core#595: secondary claude ballots (`claude#2`) run under `<base>/claude-2`, which
        // `ensure_shared_worker_fence` does not cover. Write a matching fence for every
        // secondary instance before the ballot runs — fail closed on error, as above.
        for cli in clis.iter().filter(|c| is_claude_ballot(c)) {
            if wicked_apps_core::spawn::seat_cli_key(&cli.key) != cli.key.as_str() {
                crate::execute_wrapped::ensure_secondary_instance_fence(&cli.key).map_err(|e| {
                    anyhow::anyhow!(
                        "council for {session_id}: the instance fence for `{}` could not be \
                             written ({e}); refusing to convene without its deny fence",
                        cli.key
                    )
                })?;
            }
        }
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
    let routed: Vec<(Distribution, Vec<SeatBallot>)> = std::thread::scope(|s| {
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
                        .map(|(d, ballots)| {
                            (
                                Distribution {
                                    seat_constraint: constraint,
                                    ..d
                                },
                                ballots,
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
    // (F-7R2-006 rule 2 / F-7R3-001) The BALLOT LEDGER: every convened seat's outcome on every
    // council, tallied across the distribution; a seat the ledger finds dead for the run is
    // benched — every later routing decision here (and, persisted, every dispatch) skips it.
    let mut dists: Vec<Distribution> = Vec::with_capacity(routed.len());
    let mut ledger: std::collections::BTreeMap<String, SeatLedger> =
        std::collections::BTreeMap::new();
    for (dist, ballots) in routed {
        for b in ballots {
            ledger.entry(b.cli).or_default().record(b.failure);
        }
        dists.push(dist);
    }
    let threshold = wicked_council::dispatch::seat_bench_threshold() as usize;
    for (cli, tally) in &ledger {
        let Some(reason) = tally.dead_seat_reason(threshold) else {
            continue;
        };
        if crate::domain::bench_seat(
            &mut benched,
            BenchedSeat {
                cli: cli.clone(),
                reason: reason.clone(),
                source: "ballot".to_string(),
            },
        ) {
            eprintln!(
                "wicked-core: seat '{cli}' is dead for {session_id} on its council ballots \
                 ({reason}); benched for the run (F-7R2-006 / F-7R3-001)"
            );
        }
    }
    let still_eligible: Vec<String> = eligible_seats(clis, &benched)
        .into_iter()
        .map(|c| c.key.clone())
        .collect();
    if still_eligible.is_empty() {
        // (D-10) Every seat benched on its own council ballots — typed, bench as data (above).
        // The skills-constraint refusal BELOW stays a plain error: it is NOT all-benched (a live
        // council seated the other units) and fails the run as today (DES-L3 r2 F1).
        return Err(crate::NoEligibleSeat {
            run_id: session_id.to_string(),
            benched: crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default(),
            benched_seats: benched.clone(),
        }
        .into());
    }
    // A unit the council handed to a seat the ledger then benched is moved to the first
    // still-eligible seat its skills admit — routing `degraded`, naming both seats and the cause.
    for ((unit, dist), candidates) in units.iter().zip(dists.iter_mut()).zip(candidates.iter()) {
        if unit.tool_cmd.is_some() || still_eligible.contains(&dist.assigned_cli) {
            continue;
        }
        let admits = |k: &String| match candidates {
            Some((eligible, _)) => eligible.iter().any(|c| &c.key == k),
            None => true,
        };
        let bench_reason = benched
            .iter()
            .find(|b| b.cli == dist.assigned_cli)
            .map(|b| b.reason.clone())
            .unwrap_or_else(|| "benched".to_string());
        let Some(alt) = still_eligible.iter().find(|k| admits(k)) else {
            anyhow::bail!(
                "unit {} of {session_id} cannot be seated: the council picked '{}', which {} on \
                 its ballot ({bench_reason}), and no still-eligible seat its skills admit \
                 remains ({})",
                unit.ord,
                dist.assigned_cli,
                bench_verb(&bench_reason),
                crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
            );
        };
        let was = std::mem::replace(&mut dist.assigned_cli, alt.clone());
        dist.assigned_invocation = invocation_of(clis, alt);
        dist.council_task_ref = None;
        dist.routing = RoutingInfo::Degraded {
            reason: format!(
                "council picked '{was}', which {} on its ballot ({bench_reason}); reassigned to \
                 '{alt}'",
                bench_verb(&bench_reason)
            ),
        };
    }
    let (same_seat, same_cli_instance) =
        enforce_evaluator_distinct(units, &mut dists, &still_eligible, clis, &candidates);
    // (AC-3 / core#537, core#560) When a benched seat — from ANY source (launcher health probe,
    // ballot ledger, or worker transcript) — made evaluator≠creator unsatisfiable, fail CLOSED:
    // the operator must relaunch once the seat recovers. A silent creator_seat fallback lets a
    // compromised or broken seat evaluate its own work — the fence exists to prevent that.
    // A bench-free roster that is simply too small is unchanged: when benched is empty, the
    // condition is false and the pre-existing creator_seat fallback applies.
    if !same_seat.is_empty() && !benched.is_empty() {
        let blocked: Vec<u32> = units
            .iter()
            .filter(|u| u.tool_cmd.is_none() && same_seat.contains(&u.ord))
            .map(|u| u.ord)
            .collect();
        return Err(crate::NoEligibleSeat {
            run_id: session_id.to_string(),
            benched: format!(
                "evaluator\u{2260}creator unsatisfiable for unit(s) {:?}: distinct seat(s) \
                 benched; {}",
                blocked,
                crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
            ),
            benched_seats: benched.clone(),
        }
        .into());
    }
    // (F-7R2-006 rule 3) `degradedReason` on EVERY unit whenever eligible < configured.
    //
    // There is NO same-seat `degradedReason` disclosure here any more (core#567). F-7R3-001 used to
    // disclose-and-continue when a bench left a review/test unit no seat distinct from its creator;
    // core#560 replaced that policy with the refusal above, so the state the disclosure described
    // is refused before this loop and the disclosure was dead code — it required `!benched
    // .is_empty()`, which the refusal makes impossible here. A BENCH-FREE roster with no distinct
    // seat never satisfied that condition either, so nothing it does is changed: it keeps the
    // `distinctnessFallback` field below and the gate's own UNGATED disclosure, plus — when the
    // roster has two or more seats and every one of them built — the stderr warning
    // `enforce_evaluator_distinct` prints. A ONE-seat roster is silent by design (it has nothing to
    // separate), so "bench-free" is not the same as "warned about".
    let summary = crate::domain::benched_summary(&benched, configured.len());
    for (u, d) in units.iter().zip(dists.iter_mut()) {
        d.benched = benched.clone();
        // (core#461) The evaluator≠creator fallback is a FIELD, not prose alone: a consumer keys
        // on `distinctnessFallback == "creator_seat"` for a review/test unit left on a seat that
        // built what it checks. What IS guaranteed here is only that the roster is BENCH-FREE:
        // every bench-induced distinctness failure — launcher, ballot or worker (core#560) — is
        // fail-closed above, so `benched` is necessarily empty whenever `same_seat` is not. It is
        // NOT guaranteed that the roster was too small. `enforce_evaluator_distinct` also yields
        // `same_seat` on a multi-seat roster whose every seat was assigned a Build/Recon unit, and
        // on one whose only non-builder seats this unit's skills refuse — both bench-free, both
        // landing here (review F2 on #452 covers the one-seat shape only).
        //
        // (core#591) A SECOND value beside it: an evaluator on a seat distinct from every builder
        // seat whose CLI is nonetheless a builder's CLI (`claude#2` grading `claude#1`) is
        // `same_cli_instance` — real separation of context, no separation of model. `creator_seat`
        // dominates; `enforce_evaluator_distinct` returns the two sets already disjoint.
        d.distinctness_fallback = if u.tool_cmd.is_some() {
            None
        } else if same_seat.contains(&u.ord) {
            Some(DISTINCTNESS_FALLBACK_CREATOR_SEAT.to_string())
        } else if same_cli_instance.contains(&u.ord) {
            Some(DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE.to_string())
        } else {
            None
        };
        let mut parts: Vec<String> = Vec::new();
        match &d.routing {
            RoutingInfo::Tool => {
                d.degraded_reason = None;
                continue;
            }
            RoutingInfo::Degraded { reason } => parts.push(reason.clone()),
            RoutingInfo::Council { .. } | RoutingInfo::EvaluatorDistinct { .. } => {}
        }
        if let Some(s) = &summary {
            parts.push(s.clone());
        }
        d.degraded_reason = (!parts.is_empty()).then(|| parts.join("; "));
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
/// (core#591) `key` is a seat INSTANCE key; both identity resolvers are asked about the seat's
/// CLI ([`model_of`]), because this predicate is about the MODEL — whether the ladder would refuse
/// a Claude-only unit on this seat. Reading the registry by the instance key would find no record
/// for `claude#2` and classify a claude instance as `Other`, so a second claude instance could
/// never take a Claude-only review. The INVOCATION stays the instance's own (`invocation_of(clis,
/// key)`): what the seat EXECUTES is per-instance, what it IS is per-cli.
fn seat_is_claude(clis: &[AgenticCli], key: &str) -> bool {
    use crate::skills_snapshot::WorkerCli;
    #[cfg(test)]
    SEAT_JUDGEMENTS.with(|n| n.set(n.get() + 1));
    let cli_key = model_of(key);
    let acp = crate::acp_runner::acp_seat_identity(cli_key);
    let wrapped = crate::execute_wrapped::wrapped_seat_identity(cli_key, invocation_of(clis, key));
    matches!(acp, WorkerCli::Claude) && matches!(wrapped, WorkerCli::Claude)
}

/// METHODOLOGY: evaluator ≠ creator. A REVIEW/TEST unit must not run on a CLI that produced the work
/// it checks, so after distribution we reassign any review/test unit whose council-picked CLI matches
/// a build/recon CLI to a roster seat NOT used for building (when the roster has the seats to do so)
/// — a seat the unit's skills ADMIT (core#401): a Claude-only review unit is never moved onto a seat
/// the ladder would refuse it on; with no such alternative it stays where the council put it.
/// Returns TWO ord sets (core#591):
///
/// * `same_seat` — the review/test units that STAY on a builder seat because no eligible seat
///   distinct from the builders admits them (F-7R3-001). The caller REFUSES the plan when a bench
///   is what emptied the pool (`NoEligibleSeat`, core#560): the run parks at the dead-seat gate
///   instead of executing, so there is no per-unit disclosure to make. On a BENCH-FREE roster it
///   keeps the unit where it is and sets `distinctness_fallback: "creator_seat"` instead.
/// * `same_cli_instance` — the review/test units that ARE on a seat distinct from every builder
///   SEAT, but whose seat runs the same CLI as a builder (`claude#2` grading `claude#1`'s work).
///   The separation is real but partial: no context contamination, the same model-level blind
///   spots. The caller discloses it as `distinctness_fallback: "same_cli_instance"`. Disjoint from
///   `same_seat` by construction — a unit still on a builder seat is in `same_seat` only.
///
/// Seats are compared by INSTANCE key for the reassignment (two instances of one cli ARE two
/// seats, and pooling them is the point of core#591) and by CLI key ([`model_of`]) for the
/// disclosure, because that is the axis the disclosure is about.
fn enforce_evaluator_distinct(
    units: &[WorkUnit],
    dists: &mut [Distribution],
    roster_keys: &[String],
    clis: &[AgenticCli],
    candidates: &[Candidates],
) -> (Vec<u32>, Vec<u32>) {
    use crate::domain::StageKind;
    let mut same_seat: Vec<u32> = Vec::new();
    let builder_clis: std::collections::HashSet<String> = units
        .iter()
        .zip(dists.iter())
        .filter(|(u, _)| matches!(u.stage, StageKind::Build | StageKind::Recon))
        .map(|(_, d)| d.assigned_cli.clone())
        .collect();
    // The MODELS behind those seats — `{claude}` for builders on `claude#1` and `claude#2` alike.
    let builder_models: std::collections::HashSet<&str> =
        builder_clis.iter().map(|k| model_of(k)).collect();
    if builder_clis.is_empty() {
        return (same_seat, Vec::new()); // nothing built ⇒ nothing to be distinct from
    }
    // Warn when every roster seat is a builder CLI so operators can detect degraded separation.
    // `find` below will return `None` for every Review/Test unit in this configuration, leaving
    // them on their original (builder) CLI with no routing change — silently, unless we speak up.
    // A SINGLE-seat roster never warns: it has nothing to separate and never did (review F2 on
    // #452 — main's `len() < 2` guard, kept as `warns_about_missing_evaluator_seat`).
    if warns_about_missing_evaluator_seat(roster_keys, &builder_clis) {
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
            match roster_keys
                .iter()
                .find(|k| !builder_clis.contains(*k) && admits(k))
            {
                Some(alt) => {
                    let was = std::mem::replace(&mut d.assigned_cli, alt.clone());
                    d.assigned_invocation = invocation_of(clis, alt);
                    d.routing = RoutingInfo::EvaluatorDistinct {
                        winner: alt.clone(),
                        was,
                    };
                }
                None => same_seat.push(u.ord),
            }
        }
    }
    // (core#591) The disclosure pass, over the FINAL seats — it must see where each review/test
    // unit actually landed, whether the council put it there or the reassignment above did. A unit
    // on a seat distinct from every builder SEAT whose CLI is nonetheless a builder's CLI is
    // instance-distinct, not model-distinct, and the operator must be able to see that on the
    // wire. `same_seat` units are excluded: `creator_seat` is the stronger, dominating disclosure.
    //
    // `!builder_clis.contains(&d.assigned_cli)` is not redundant with `!same_seat.contains(&u.ord)`:
    // the loop above zips `units` with `candidates`, so a unit past the end of a SHORTER
    // `candidates` is never visited by it and can sit on a builder seat without being in
    // `same_seat`. Stating the condition here makes the predicate mean what it says — a seat
    // distinct from every builder SEAT — instead of inheriting that from another loop's bounds.
    let same_cli_instance: Vec<u32> = units
        .iter()
        .zip(dists.iter())
        .filter(|(u, d)| {
            u.tool_cmd.is_none()
                && matches!(u.stage, StageKind::Review | StageKind::Test)
                && !same_seat.contains(&u.ord)
                && !builder_clis.contains(&d.assigned_cli)
                && builder_models.contains(model_of(&d.assigned_cli))
        })
        .map(|(u, _)| u.ord)
        .collect();
    (same_seat, same_cli_instance)
}

/// Whether [`enforce_evaluator_distinct`] warns that evaluator≠creator cannot be enforced: a
/// roster of TWO or more seats, every one of which built. One seat has nothing to separate and
/// stays silent — main's `len() < 2` guard, kept (review F2 on #452).
fn warns_about_missing_evaluator_seat(
    roster_keys: &[String],
    builder_clis: &std::collections::HashSet<String>,
) -> bool {
    roster_keys.len() >= 2 && roster_keys.iter().all(|k| builder_clis.contains(k))
}

/// (F-7R3-001) One convened seat's outcome on one unit's council, as the run-level bench reads it.
#[derive(Debug, Clone)]
struct SeatBallot {
    cli: String,
    /// `None` — the seat voted. `Some` — it produced no vote: the dispatch branch it took and the
    /// cause the council classified from its own words, when any.
    failure: Option<(
        wicked_council::types::SeatFailureKind,
        Option<wicked_council::types::SeatFailureReason>,
    )>,
}

/// (F-7R3-001) One seat's tally across every council of a distribution — the evidence the
/// run-level bench decides on.
#[derive(Debug, Default)]
struct SeatLedger {
    voted: usize,
    failures: Vec<(
        wicked_council::types::SeatFailureKind,
        Option<wicked_council::types::SeatFailureReason>,
    )>,
}

impl SeatLedger {
    fn record(
        &mut self,
        failure: Option<(
            wicked_council::types::SeatFailureKind,
            Option<wicked_council::types::SeatFailureReason>,
        )>,
    ) {
        match failure {
            None => self.voted += 1,
            Some(f) => self.failures.push(f),
        }
    }

    fn count_reason(&self, reason: wicked_council::types::SeatFailureReason) -> usize {
        self.failures
            .iter()
            .filter(|(_, r)| *r == Some(reason))
            .count()
    }

    /// Why this seat is DEAD for the run — the bench reason — or `None` while it may still take
    /// work. Deny-dominates in both directions (F-7R3-001):
    ///
    /// - `not_logged_in` / `not_installed` on ANY ballot bench on first occurrence: a sign-in or a
    ///   binary does not appear mid-run (the F-7R2-006 rule, extended to the missing binary).
    /// - Otherwise a seat that VOTED at least once is kept — one quota refusal beside a vote is a
    ///   flaky provider, not a dead seat — and a seat whose failures include one the council could
    ///   not classify is kept too: an unknown failure is not proof of a dead seat.
    /// - The dispatcher's own health-gate abstention (`Benched`, core#461) is NOT unclassified: it
    ///   says the seat failed consecutively and is sitting out. It corroborates a dead-class ballot
    ///   beside it (the smoke S04 shape — round 1 `quota_exhausted`, round 2 `Benched` — benches
    ///   `quota_exhausted (1/2 ballots)`) and proves nothing on its own: a seat that only ever
    ///   abstained was never asked.
    /// - With no vote and every failure dead-class or an abstention: `quota_exhausted` if any
    ///   ballot said so; else `timed_out` once the timeouts plus the abstentions that followed them
    ///   reach `threshold` (the dispatcher's own consecutive streak, `seat_bench_threshold`) — a
    ///   seat charging its whole budget to every ballot it answered and then benched for it.
    ///
    /// The count that proved it rides the reason (`quota_exhausted (3/3 ballots)`), so
    /// `degradedReason` names the seat, the kind and the evidence.
    fn dead_seat_reason(&self, threshold: usize) -> Option<String> {
        use wicked_council::types::{SeatFailureKind, SeatFailureReason};
        let asked = self.voted + self.failures.len();
        if self.count_reason(SeatFailureReason::NotLoggedIn) > 0 {
            return Some(SeatFailureReason::NotLoggedIn.as_str().to_string());
        }
        let missing = self.count_reason(SeatFailureReason::NotInstalled);
        if missing > 0 {
            return Some(format!(
                "{} ({missing}/{asked} ballots)",
                SeatFailureReason::NotInstalled.as_str()
            ));
        }
        if self.voted > 0 || self.failures.is_empty() {
            return None;
        }
        let quota = self.count_reason(SeatFailureReason::QuotaExhausted);
        let timeouts = self
            .failures
            .iter()
            .filter(|(k, r)| *k == SeatFailureKind::TimedOut && r.is_none())
            .count();
        let abstained = self
            .failures
            .iter()
            .filter(|(k, _)| *k == SeatFailureKind::Benched)
            .count();
        // (R5b / DES-L3 3D′) An UNCLASSIFIED PERSISTENT failure — the seat exited non-zero on
        // every ballot it was asked, said nothing the engine recognises, and never voted — is
        // dead for this run once it reaches the ballot threshold, mirroring the timeout rule
        // below. crew used to carry this class across runs in its own 30-min council-count
        // ledger; that ledger is deleted (one bench ledger), so the engine judges it here, per
        // run. One unclassified exit beside a vote stays "flaky, not dead" (`voted > 0` above);
        // a mixed bag (unclassified + quota, say) still proves nothing (the `None` below).
        let unclassified = self
            .failures
            .iter()
            .filter(|(k, r)| *k == SeatFailureKind::NonZeroExit && r.is_none())
            .count();
        if unclassified > 0
            && unclassified + abstained == self.failures.len()
            && unclassified + abstained >= threshold.max(1)
        {
            return Some(format!(
                "non_zero_exit ({unclassified}/{asked} ballots, unclassified)"
            ));
        }
        // Any other unclassified failure still proves nothing; abstentions alone are not evidence.
        if quota + timeouts + abstained != self.failures.len() || quota + timeouts == 0 {
            return None;
        }
        if quota > 0 {
            return Some(format!(
                "{} ({quota}/{asked} ballots)",
                SeatFailureReason::QuotaExhausted.as_str()
            ));
        }
        (timeouts + abstained >= threshold.max(1))
            .then(|| format!("timed_out ({timeouts}/{asked} ballots, no vote returned)"))
    }
}

/// The verb a bench reason reads as in a routing sentence — `failed authentication`, `exhausted
/// its quota`, `is not installed`, `timed out`; `was benched` for a launcher's own words.
fn bench_verb(reason: &str) -> &'static str {
    use wicked_council::types::SeatFailureReason as R;
    for r in [R::NotLoggedIn, R::QuotaExhausted, R::NotInstalled] {
        if reason.starts_with(r.as_str()) {
            return r.verb();
        }
    }
    if reason.starts_with("timed_out") {
        "timed out"
    } else {
        "was benched"
    }
}

/// Route one unit. Returns the distribution AND every convened seat's ballot outcome — the
/// caller's ledger decides which seats are dead for the run (F-7R2-006 / F-7R3-001). The
/// single-seat path convenes nothing and reports nothing.
fn distribute_one(
    unit: &WorkUnit,
    clis: &[AgenticCli],
    roster_keys: &[String],
    session_id: &str,
    db_path: Option<&str>,
    dispatcher: &Arc<dyn Dispatcher + Send + Sync>,
    relay: Option<EventRelay>,
) -> anyhow::Result<(Distribution, Vec<SeatBallot>)> {
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
                distinctness_fallback: None,
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
    let ballots = status
        .as_ref()
        .map(|s| seat_ballots(s, roster_keys))
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
            distinctness_fallback: None,
        },
        ballots,
    ))
}

/// (F-7R2-006 / F-7R3-001 / core#461) Every convened seat's outcome on EVERY ballot of this
/// council, as the run-level bench tallies it — one [`SeatBallot`] per seat per round. A seat with
/// a failure record on a round failed it (its dispatch branch and the cause the council classified
/// from its own words — `SeatFailure::reason`); a convened seat WITHOUT one voted on that round —
/// a round is complete when polled (`queue_blocking` joined the council) and a convened seat is
/// accounted for exactly once per round, in the votes or in the failures. The dispatcher's own
/// health-gate ABSTENTIONS (`SeatFailureKind::Benched`) are recorded as what they are: the ledger
/// reads one as corroboration of a dead-class ballot beside it and as proof of nothing on its own
/// ([`SeatLedger::dead_seat_reason`]). They used to be dropped here, and the council reported the
/// LATEST round only — so a seat that failed round 1 on quota and was health-gated for the runoff
/// reached the ledger as no evidence at all and stayed routable (core#461, smoke S04). A council
/// that never ran (still queued / running when polled) yields no evidence; a status that kept no
/// history (a hand-built one) falls back to its latest round.
fn seat_ballots(status: &PollStatus, convened: &[String]) -> Vec<SeatBallot> {
    if matches!(status.state, TaskState::Queued | TaskState::Running) {
        return Vec::new();
    }
    let latest = std::slice::from_ref(&status.seat_failures);
    let rounds = if status.seat_failure_history.is_empty() {
        latest
    } else {
        status.seat_failure_history.as_slice()
    };
    rounds
        .iter()
        .flat_map(|failures| {
            convened.iter().map(move |cli| SeatBallot {
                cli: cli.clone(),
                failure: failures
                    .iter()
                    .find(|f| &f.cli == cli)
                    .map(|f| (f.failure.kind, f.failure.reason)),
            })
        })
        .collect()
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

    /// core#595 HIGH-3: convening a council that seats a SECONDARY claude ballot (`claude#2`)
    /// must write the deny fence to the INSTANCE home (`<worker>/claude-2/settings.json`).
    /// `ensure_shared_worker_fence` writes only `<worker>/claude`; before this fix nothing
    /// wrote to `claude-2`, so the ballot started unfenced.
    ///
    /// FAILS on base (c163cf9): `<worker>/claude-2/settings.json` does not exist.
    #[test]
    fn convening_a_secondary_claude_ballot_writes_fence_to_the_instance_home() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let hatch = crate::execute_wrapped::INHERIT_OPERATOR_CONFIG_ENV;
        let prev_hatch = std::env::var_os(hatch);
        std::env::remove_var(hatch);
        let base = std::env::temp_dir().join(format!(
            "wdistribute-secondary-fence-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &base);

        let unit = WorkUnit::pending("u1", "s1", 0, "Write the parser module");
        let seen: Arc<std::sync::Mutex<Vec<AgenticCli>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher {
            seen: Arc::clone(&seen),
        });

        distribute_units_on(
            std::slice::from_ref(&unit),
            &[claude_seat("claude#2")],
            "s2",
            None,
            &dispatcher,
            None,
            None,
        )
        .expect("distribute a roster with a secondary claude ballot");

        assert!(
            base.join("claude-2").join("settings.json").exists(),
            "the fence must be written at the secondary instance home \
             (<worker>/claude-2/settings.json); it is missing — the ballot ran without a deny fence"
        );
        // Assert specific security-critical deny rules are present.
        // Hardcoded — NOT computed from shared_deny_rules — so a missing rule in that
        // function fails this test rather than silently passing. (core#595 evaluator CRITICAL)
        let bytes = std::fs::read(base.join("claude-2").join("settings.json")).unwrap();
        let settings: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let deny: Vec<String> = settings["permissions"]["deny"]
            .as_array()
            .expect("permissions.deny present")
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        // Directory fences: derive the expected path from HOME directly, never from
        // shared_deny_rules — the point is to catch a missing rule in that function.
        // Fence rules spell every path with forward slashes on every OS (the documented Windows
        // form is `C:/Users/me/.ssh/**`, execute_wrapped.rs tests). A raw USERPROFILE is
        // `C:\Users\...`, so normalise the separator HERE with a plain replace — never through
        // `rule_path`, which is the code under test and would make this assertion vacuous again.
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_default()
            .replace('\\', "/");
        for must_contain in [
            // Credential and key protection: these are the rules that stop reads of SSH keys,
            // AWS credentials and git credential files by a worker running in the secondary home.
            format!("Read({home}/.ssh/**)"),
            format!("Edit({home}/.ssh/**)"),
            format!("Read({home}/.aws/**)"),
            format!("Edit({home}/.aws/**)"),
            format!("Read({home}/.git-credentials)"),
            format!("Edit({home}/.git-credentials)"),
            // Operator tool-config fences: council config and wicked daemon state.
            format!("Read({home}/.config/wicked-council/**)"),
            format!("Edit({home}/.config/wicked-council/**)"),
            // Privilege escalation and remote-write Bash verbs — static, never HOME-dependent.
            "Bash(sudo:*)".to_string(),
            "Bash(git push:*)".to_string(),
            "Bash(gh pr create:*)".to_string(),
        ] {
            assert!(
                deny.contains(&must_contain),
                "secondary fence at claude-2/settings.json is missing security-critical rule \
                 {must_contain:?}; full deny list: {deny:#?}"
            );
        }

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
            seat_failure_history: vec![],
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

    /// core#468: the run's BASE skill is EXISTENCE-only — it never joins the seat requirement, so a
    /// `portable: false` base skill neither narrows the candidates onto claude nor records a
    /// constraint: the council convenes over the WHOLE roster exactly as for a skill-free unit,
    /// with or without a portable phase `skill_ref` beside it. Mutation: feed `base_skill_ref`
    /// into `seat_requirement` inside `seat_candidates` and both units collapse onto claude with
    /// a constraint naming `wicked-garden-repo-learn`.
    #[test]
    fn a_nonportable_base_skill_never_narrows_seating_onto_claude() {
        let (_home, _env, _) = hermetic_home("route-base-skill-home");
        let snapshot = published("route-base-skill");
        let roster = [
            seat_running("copilot", "copilot"),
            seat_running("claude", "claude"),
            seat_running("pi", "pi"),
        ];
        // The live shape: `wicked-garden-repo-learn` is `portable: false` in this generation.
        let mut bare = WorkUnit::pending("u1", "s1", 1, "Triage the report");
        bare.base_skill_ref = Some("wicked-garden-repo-learn".to_string());
        let mut with_phase_skill = skilled(2, "wicked-garden-search");
        with_phase_skill.base_skill_ref = Some("wicked-garden-repo-learn".to_string());

        let (dispatcher, calls) = spy();
        let (relay, convened) = convened_seats();
        let dists = distribute_units_against(
            &[bare, with_phase_skill],
            &roster,
            "s1",
            None,
            &dispatcher,
            Some(relay),
            Some(&snapshot),
            None,
        )
        .expect("a base skill constrains nothing at distribution");
        assert_eq!(dists.len(), 2);
        for (d, ord) in dists.iter().zip([1u32, 2]) {
            assert!(
                d.seat_constraint.is_none(),
                "unit {ord}: no constraint is recorded, got {:?}",
                d.seat_constraint
            );
            assert_eq!(
                d.assigned_cli, "copilot",
                "unit {ord}: the spy votes option 1 of the FULL roster"
            );
        }
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "the council genuinely convened for both units"
        );
        let full: Vec<String> = vec!["copilot".into(), "claude".into(), "pi".into()];
        // The two councils report in whatever order their threads land; the claim is about WHICH
        // seats each convened over, not their interleaving.
        let mut seen = convened.lock().unwrap().clone();
        seen.sort_by_key(|(ord, _)| *ord);
        assert_eq!(
            seen,
            vec![(1, full.clone()), (2, full)],
            "…over every roster seat, both times"
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
        // (D-10) The SAME typed refusal the intake raises, carrying the bench as data, with the
        // intake's byte-identical `Display` (crew's parser keys on it).
        let refusal = err
            .downcast_ref::<crate::NoEligibleSeat>()
            .expect("an all-benched roster is a typed NoEligibleSeat, not a bare message");
        assert_eq!(refusal.run_id, "s1");
        assert_eq!(refusal.benched_seats.len(), 2);
        assert_eq!(
            err.to_string(),
            "no eligible seat for s1: 2 of 2 seats benched: codex (signed out — launcher), pi \
             (dispatch budget — launcher) — sign a seat in, or add one, before launching"
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
        let refusal = err
            .downcast_ref::<crate::NoEligibleSeat>()
            .expect("typed NoEligibleSeat");
        assert_eq!(refusal.benched_seats[0].cli, "codex");
        assert!(
            err.to_string()
                .contains("1 of 1 seats benched: codex (signed out — launcher)"),
            "{err}"
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
        assert!(
            dists[1].distinctness_fallback.is_none(),
            "separated — no fallback to disclose: {:?}",
            dists[1].distinctness_fallback
        );
    }

    // ── F-7R3-001: the ballot ledger — a seat dead for the run is never an evaluator target ──

    /// A stub whose named seat fails every ballot the way `failure` says while every other seat
    /// votes for option 1 (the first roster seat); `fail_first_only` makes the named seat fail
    /// ONCE and vote afterwards — the mixed record.
    struct DeadSeat {
        seat: &'static str,
        failure: fn() -> wicked_council::types::SeatFailure,
        fail_first_only: bool,
        failed: AtomicUsize,
    }
    impl Dispatcher for DeadSeat {
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
            _ctx: &wicked_council::types::BallotContext,
        ) -> wicked_council::types::DispatchOutcome {
            if cli.key == self.seat {
                let n = self.failed.fetch_add(1, Ordering::SeqCst);
                if !self.fail_first_only || n == 0 {
                    return wicked_council::types::DispatchOutcome::Failed((self.failure)());
                }
            }
            wicked_council::types::DispatchOutcome::Voted(self.dispatch(cli, task).unwrap())
        }
    }
    fn dead_seat(
        seat: &'static str,
        failure: fn() -> wicked_council::types::SeatFailure,
        fail_first_only: bool,
    ) -> Arc<dyn Dispatcher + Send + Sync> {
        Arc::new(DeadSeat {
            seat,
            failure,
            fail_first_only,
            failed: AtomicUsize::new(0),
        })
    }
    /// The copilot refusal from run c7e42297, as a non-zero exit with the words on stderr.
    fn quota_refusal() -> wicked_council::types::SeatFailure {
        wicked_council::types::SeatFailure::new(
            wicked_council::types::SeatFailureKind::NonZeroExit,
            "exit 1",
        )
        .with_output(
            "",
            "Error: You have exceeded your monthly quota for premium requests.",
        )
    }
    fn timed_out_ballot() -> wicked_council::types::SeatFailure {
        wicked_council::types::SeatFailure::new(
            wicked_council::types::SeatFailureKind::TimedOut,
            "exceeded 60s dispatch budget",
        )
    }
    /// A council-routed distribution on `key`, for the routing passes that take `dists` directly.
    fn council_dist(key: &str, invocation: &str) -> Distribution {
        Distribution {
            assigned_cli: key.into(),
            assigned_invocation: Some(invocation.into()),
            council_task_ref: None,
            routing: RoutingInfo::Council {
                winner: key.into(),
                agreement_pct: 100,
                returned: 1,
                seated: Some(1),
                dissent: 0,
            },
            seat_constraint: None,
            degraded_reason: None,
            benched: Vec::new(),
            distinctness_fallback: None,
        }
    }

    /// (core#591 collision 1) Two instances of ONE cli are TWO seats, so the review unit leaves
    /// the seat that built what it checks.
    ///
    /// PREMISE, verified on origin/main before the change: spelled the only way main's roster
    /// could spell two instances — two records with the SAME `key` — `builder_clis` (a
    /// `HashSet<String>` of `assigned_cli`) collapsed both to one entry and this returned
    /// `same_seat == [2]` with the review left on `"claude"`. Under #591 the second instance is a
    /// different key, so the set holds two entries and the fence has a seat to move the review to.
    ///
    /// The expected values are the methodology rule written out (evaluator ≠ creator ⇒ no unit in
    /// `same_seat`, and the review's seat is not the build's), never read back from the predicate.
    #[test]
    fn two_instances_of_one_cli_are_two_seats_not_one() {
        let units = build_and_review();
        let mut dists = [
            council_dist("claude", "claude -p {PROMPT}"),
            council_dist("claude", "claude -p {PROMPT}"),
        ];
        let clis = [seat("claude"), seat("claude#2")];
        let roster_keys = vec!["claude".to_string(), "claude#2".to_string()];
        let (same, same_cli) =
            enforce_evaluator_distinct(&units, &mut dists, &roster_keys, &clis, &[None, None]);
        assert_eq!(
            same,
            Vec::<u32>::new(),
            "a SECOND instance of claude is a second seat: the review must not stay on the seat \
             that built it (review landed on {:?})",
            dists[1].assigned_cli
        );
        assert_eq!(dists[0].assigned_cli, "claude", "the build is untouched");
        assert_eq!(
            dists[1].assigned_cli, "claude#2",
            "the review moves to the second instance"
        );
        assert_eq!(
            dists[1].assigned_invocation.as_deref(),
            Some("run-claude#2 {PROMPT}"),
            "…and carries THAT instance's own launch template, not the first record's"
        );
        // …and the separation it got is disclosed for what it is: same model, different instance.
        assert_eq!(
            same_cli,
            vec![2],
            "an instance-distinct evaluator on the creator's cli is disclosed"
        );
    }

    /// (core#591 collision 2) A roster that names one seat key twice is REFUSED at plan time.
    ///
    /// PREMISE, verified on origin/main before the change: `invocation_of` resolves with
    /// `.find(|c| c.key == key)`, so two records sharing a key silently resolved to the FIRST —
    /// `invocation_of(&clis, "claude")` returned `Some("claude --instance-a {PROMPT}")` and the
    /// second record's template was unreachable. Nothing refused the roster. The fix is the
    /// refusal, not a disambiguation rule: an identifier that names two different seats is not an
    /// identifier.
    #[test]
    fn a_duplicate_seat_key_is_refused_at_plan_time() {
        let mut a = seat("claude");
        a.headless_invocation = "claude --instance-a {PROMPT}".into();
        let mut b = seat("claude");
        b.headless_invocation = "claude --instance-b {PROMPT}".into();

        let err = refuse_duplicate_seat_keys(&[a.clone(), b.clone()])
            .expect_err("a duplicate seat key must be refused");
        let msg = err.to_string();
        assert!(msg.contains("'claude'"), "the refusal names the key: {msg}");
        assert!(
            msg.contains("claude#2"),
            "…and the remedy, so an operator does not have to guess the spelling: {msg}"
        );

        // The refusal runs on the WHOLE-PLAN path, before any routing — including for an all-tool
        // plan, whose roster is just as wrong.
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SpyDispatcher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        for units in [build_and_review().to_vec(), vec![tool_unit(1, "index")]] {
            let err = distribute_units_against_benched(
                &units,
                &[a.clone(), b.clone()],
                "s1",
                None,
                &dispatcher,
                None,
                None,
                None,
                &[],
            )
            .expect_err("the plan is refused before anything routes");
            assert!(err.to_string().contains("more than once"), "{err}");
        }

        // The distinct spelling is accepted, and each instance resolves its OWN template.
        let mut two = b.clone();
        two.key = "claude#2".into();
        refuse_duplicate_seat_keys(&[a.clone(), two.clone()]).expect("two instances, two keys");
        assert_eq!(
            invocation_of(&[a.clone(), two.clone()], "claude").as_deref(),
            Some("claude --instance-a {PROMPT}")
        );
        assert_eq!(
            invocation_of(&[a, two], "claude#2").as_deref(),
            Some("claude --instance-b {PROMPT}")
        );
    }

    /// (core#591 S3, BOTH directions) The `distinctnessFallback` disclosure separates the two
    /// kinds of evaluator distinctness that are NOT the same thing:
    ///
    /// * a MODEL-distinct evaluator (codex grading claude) discloses NOTHING — it is the
    ///   methodology working;
    /// * an INSTANCE-distinct evaluator (`claude#2` grading `claude#1`) discloses
    ///   `same_cli_instance` — context separated, model not.
    ///
    /// The negative half is the one that matters: a disclosure that fires on every reassignment
    /// carries no information. Both expected values are the governance rule written out, not read
    /// back from the routing.
    #[test]
    fn an_instance_distinct_evaluator_discloses_and_a_model_distinct_one_does_not() {
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let fallback_of = |roster: &[AgenticCli]| -> (Option<String>, String) {
            let dists = distribute_units_against_benched(
                &build_and_review(),
                roster,
                "s1",
                None,
                &dispatcher,
                None,
                None,
                None,
                &[],
            )
            .expect("routes");
            assert_eq!(
                dists[0].distinctness_fallback, None,
                "the build is no evaluator"
            );
            (
                dists[1].distinctness_fallback.clone(),
                dists[1].assigned_cli.clone(),
            )
        };

        // Instance-distinct: two seats, ONE cli.
        let (fallback, seat_key) = fallback_of(&[seat("claude"), seat("claude#2")]);
        assert_ne!(seat_key, "claude", "the review did move off the build seat");
        assert_eq!(
            fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE),
            "an evaluator on another INSTANCE of the creator's cli must be disclosed"
        );

        // Model-distinct: two seats, TWO clis. Nothing to disclose.
        let (fallback, seat_key) = fallback_of(&[seat("claude"), seat("codex")]);
        assert_ne!(seat_key, "claude", "the review did move off the build seat");
        assert_eq!(
            fallback, None,
            "a genuinely model-distinct evaluator discloses nothing — otherwise the disclosure \
             says nothing about the run it rides"
        );
    }

    /// (core#591) `creator_seat` DOMINATES `same_cli_instance`: a review unit that never left the
    /// seat that built its work is disclosed as that, not as the weaker instance fallback. Without
    /// the dominance rule a single-instance roster (`claude` alone) would satisfy both conditions
    /// — its seat IS a builder seat AND its cli IS a builder cli — and the consumer would read the
    /// stronger failure as the weaker one.
    #[test]
    fn creator_seat_dominates_the_instance_disclosure() {
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher {
            seen: Arc::new(std::sync::Mutex::new(Vec::new())),
        });
        let dists = distribute_units_against_benched(
            &build_and_review(),
            &[seat("claude")],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect("routes");
        assert_eq!(dists[1].assigned_cli, "claude");
        assert_eq!(
            dists[1].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_CREATOR_SEAT),
            "a unit still on its creator's seat is the STRONGER disclosure"
        );
    }

    /// (core#591) `seat_is_claude` is a question about the MODEL, so it reads the cli key behind
    /// the instance key. A registry lookup by `claude#2` finds no record and would classify a
    /// claude instance as `Other` — which would silently bar every second claude instance from
    /// every Claude-only review unit, the exact pooling core#591 exists to allow.
    #[test]
    fn a_second_claude_instance_is_still_judged_a_claude_seat() {
        let first = claude_seat("claude");
        let mut second = claude_seat("claude#2");
        second.headless_invocation = "claude -p {PROMPT}".into();
        let clis = [first, second];
        assert!(seat_is_claude(&clis, "claude"), "the control");
        assert!(
            seat_is_claude(&clis, "claude#2"),
            "a second INSTANCE of claude is still a claude seat"
        );
        // …and the split it relies on is the same one the configuration home is keyed on.
        assert_eq!(model_of("claude#2"), "claude");
        assert_eq!(model_of("claude"), "claude");
        assert_eq!(model_of("codex#7"), "codex");
    }

    /// A build unit (ord 1) and the review unit (ord 2) that must not share its seat.
    fn build_and_review() -> [WorkUnit; 2] {
        let build = WorkUnit::pending("u1", "s1", 1, "Build the thing");
        let mut review = WorkUnit::pending("u2", "s1", 2, "Review the thing");
        review.stage = crate::domain::StageKind::Review;
        [build, review]
    }

    /// The rule matrix of the ledger, one case per clause — deny-dominates in both directions.
    #[test]
    fn the_ballot_ledger_benches_only_a_seat_that_is_dead_for_the_run() {
        use wicked_council::types::{SeatFailureKind as K, SeatFailureReason as R};
        let quota = Some((K::NonZeroExit, Some(R::QuotaExhausted)));
        let unclassified = Some((K::NonZeroExit, None));
        let timeout = Some((K::TimedOut, None));

        let mut l = SeatLedger::default();
        assert_eq!(l.dead_seat_reason(2), None, "no evidence, no verdict");
        l.record(quota);
        assert_eq!(
            l.dead_seat_reason(2).as_deref(),
            Some("quota_exhausted (1/1 ballots)")
        );
        l.record(None);
        assert_eq!(l.dead_seat_reason(2), None, "one vote keeps the seat");

        let mut l = SeatLedger::default();
        l.record(quota);
        l.record(unclassified);
        assert_eq!(
            l.dead_seat_reason(2),
            None,
            "a mixed bag (quota + unclassified) is not proof of a dead seat"
        );

        // (R5b / 3D′) UNCLASSIFIED and PERSISTENT: non-zero exits on every ballot asked, nothing
        // recognisable said, no vote — benched at the threshold, like a timeout streak.
        let mut l = SeatLedger::default();
        l.record(unclassified);
        assert_eq!(
            l.dead_seat_reason(2),
            None,
            "one unclassified exit is flaky, not dead"
        );
        l.record(unclassified);
        assert_eq!(
            l.dead_seat_reason(2).as_deref(),
            Some("non_zero_exit (2/2 ballots, unclassified)")
        );
        l.record(None);
        assert_eq!(
            l.dead_seat_reason(2),
            None,
            "one vote beside two unclassified exits keeps the seat"
        );

        let mut l = SeatLedger::default();
        l.record(timeout);
        assert_eq!(l.dead_seat_reason(2), None, "one timeout is a slow answer");
        l.record(timeout);
        assert_eq!(
            l.dead_seat_reason(2).as_deref(),
            Some("timed_out (2/2 ballots, no vote returned)"),
            "the dispatcher's streak, with no vote in between"
        );
        l.record(None);
        assert_eq!(
            l.dead_seat_reason(2),
            None,
            "…but a vote after the streak keeps it"
        );

        let mut l = SeatLedger::default();
        l.record(timeout);
        l.record(quota);
        assert_eq!(
            l.dead_seat_reason(2).as_deref(),
            Some("quota_exhausted (1/2 ballots)"),
            "quota beside a timeout, no vote: quota names it"
        );

        let mut l = SeatLedger::default();
        l.record(None);
        l.record(Some((K::NonZeroExit, Some(R::NotLoggedIn))));
        assert_eq!(
            l.dead_seat_reason(2).as_deref(),
            Some("not_logged_in"),
            "authentication benches on first occurrence, votes or not (F-7R2-006)"
        );

        let mut l = SeatLedger::default();
        l.record(Some((K::SpawnFailed, Some(R::NotInstalled))));
        assert_eq!(
            l.dead_seat_reason(2).as_deref(),
            Some("not_installed (1/1 ballots)")
        );

        assert_eq!(
            bench_verb("quota_exhausted (2/2 ballots)"),
            "exhausted its quota"
        );
        assert_eq!(bench_verb("not_logged_in"), "failed authentication");
        assert_eq!(
            bench_verb("timed_out (2/2 ballots, no vote returned)"),
            "timed out"
        );
        assert_eq!(bench_verb("signed out"), "was benched");
    }

    /// (a) A seat that fails EVERY ballot on quota (copilot in run c7e42297: "exceeded your
    /// monthly quota") is benched for the run: `degradedReason` names the seat, the kind and the
    /// count on every unit, and the evaluator≠creator reassignment moves the review unit PAST it.
    #[test]
    fn a_seat_whose_every_ballot_fails_on_quota_is_benched_and_never_an_evaluator_distinct_target()
    {
        let dispatcher = dead_seat("copilot", quota_refusal, false);
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
        let dists = distribute_units_against_benched(
            &build_and_review(),
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
            "the voters' option 1: {:?}",
            dists[0].routing
        );
        assert_eq!(
            dists[1].assigned_cli, "codex",
            "the review moves past the quota-dead seat: {:?}",
            dists[1].routing
        );
        assert!(matches!(
            &dists[1].routing,
            RoutingInfo::EvaluatorDistinct { winner, was } if winner == "codex" && was == "claude"
        ));
        let why = dists[1]
            .degraded_reason
            .as_deref()
            .expect("degraded: a seat is benched");
        // Every ROUND is a ballot the seat was asked (core#461): two live voters of three seated
        // are 67 % — below the bar — so each council runs the 3-round cap; two councils = 6.
        assert!(
            why.contains("1 of 3 seats benched: copilot (quota_exhausted (6/6 ballots) — ballot)"),
            "{why}"
        );
        assert_eq!(
            dists[0].degraded_reason, dists[1].degraded_reason,
            "the bench rides every unit"
        );
        assert_eq!(dists[0].benched.len(), 1);
        assert_eq!(dists[0].benched[0].cli, "copilot");
        assert_eq!(dists[0].benched[0].source, "ballot");
    }

    /// (core#461, smoke S04 / F-SMOKE-002 / F-RC2-029) The shape the wave-6 bench missed: the
    /// named seat fails ROUND 1 the way `failure` says and is then health-gated by the dispatcher —
    /// on every later round it ABSTAINS (`Benched`, the gate's own verdict, no subprocess behind
    /// it). The live seats split on round 1 (the `claude` seat votes option 1, every other live
    /// seat option 2) so the council runs a runoff, and converge on option 1 from round 2 on.
    /// With `votes_round_one` the named seat VOTES option 1 on round 1 instead of failing — the
    /// health-gated flaky seat of acceptance (d).
    struct DeadThenBenched {
        seat: &'static str,
        failure: fn() -> wicked_council::types::SeatFailure,
        votes_round_one: bool,
        rounds_seen: AtomicUsize,
    }
    impl Dispatcher for DeadThenBenched {
        fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
            Some(fit_vote(&cli.key, "1 — fit"))
        }
        fn dispatch_ballot_detailed(
            &self,
            cli: &AgenticCli,
            _task: &CouncilTask,
            ctx: &wicked_council::types::BallotContext,
        ) -> wicked_council::types::DispatchOutcome {
            use wicked_council::types::{DispatchOutcome, SeatFailure, SeatFailureKind};
            self.rounds_seen
                .fetch_max(ctx.ballot as usize, Ordering::SeqCst);
            if cli.key == self.seat {
                if ctx.ballot > 1 {
                    return DispatchOutcome::Failed(SeatFailure::new(
                        SeatFailureKind::Benched,
                        "seat benched for 29s more (span 30s) after consecutive failures; \
                         re-admission is one probationary ballot on expiry",
                    ));
                }
                if !self.votes_round_one {
                    return DispatchOutcome::Failed((self.failure)());
                }
            }
            let rec = if ctx.ballot == 1 && cli.key != "claude" && cli.key != self.seat {
                "2 — other"
            } else {
                "1 — fit"
            };
            DispatchOutcome::Voted(fit_vote(&cli.key, rec))
        }
    }
    fn fit_vote(cli: &str, recommendation: &str) -> Vote {
        Vote {
            cli: cli.to_string(),
            recommendation: recommendation.into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "test".into(),
        }
    }
    fn dead_then_benched(
        seat: &'static str,
        failure: fn() -> wicked_council::types::SeatFailure,
        votes_round_one: bool,
    ) -> Arc<DeadThenBenched> {
        Arc::new(DeadThenBenched {
            seat,
            failure,
            votes_round_one,
            rounds_seen: AtomicUsize::new(0),
        })
    }
    /// claude's refusal on stdout with stderr empty (F-030/F-031), as a non-zero exit.
    fn login_refusal() -> wicked_council::types::SeatFailure {
        wicked_council::types::SeatFailure::new(
            wicked_council::types::SeatFailureKind::NonZeroExit,
            "exit 1",
        )
        .with_output("Not logged in · Please run /login", "")
    }
    /// The binary could not be spawned at all — judged structurally from the spawn error.
    fn missing_binary() -> wicked_council::types::SeatFailure {
        wicked_council::types::SeatFailure::spawn_failed(
            "copilot",
            &std::io::Error::from(std::io::ErrorKind::NotFound),
        )
    }

    /// (core#461 acceptance a) Round 1 `quota_exhausted`, round 2 the dispatcher's own `Benched`
    /// abstention — the ledger reads the abstention as corroboration: the seat is benched for the
    /// run with the quota cause and the count that proved it, `benched_seats` is non-empty on every
    /// unit, `degradedReason` names it, and the evaluator≠creator reassignment moves the review
    /// unit PAST it onto the alternative — never onto the benched seat (F-RC2-029's mechanism).
    #[test]
    fn a_round1_quota_ballot_plus_round2_abstention_benches_the_seat() {
        let stub = dead_then_benched("copilot", quota_refusal, false);
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = stub.clone();
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
        let dists = distribute_units_against_benched(
            &build_and_review(),
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
        assert!(
            stub.rounds_seen.load(Ordering::SeqCst) >= 2,
            "the councils ran a runoff, so the abstention happened"
        );
        assert_eq!(dists[0].assigned_cli, "claude", "{:?}", dists[0].routing);
        assert_eq!(
            dists[1].assigned_cli, "codex",
            "the review moves past the quota-dead seat: {:?}",
            dists[1].routing
        );
        assert!(matches!(
            &dists[1].routing,
            RoutingInfo::EvaluatorDistinct { winner, was } if winner == "codex" && was == "claude"
        ));
        assert_eq!(
            dists[1].distinctness_fallback, None,
            "separated — no fallback"
        );
        // Two councils × (one quota ballot + one abstention) = 2 of 4 ballots, no vote.
        assert_eq!(dists[0].benched.len(), 1, "{:?}", dists[0].benched);
        assert_eq!(dists[0].benched[0].cli, "copilot");
        assert_eq!(dists[0].benched[0].source, "ballot");
        assert_eq!(dists[0].benched[0].reason, "quota_exhausted (2/4 ballots)");
        let why = dists[1]
            .degraded_reason
            .as_deref()
            .expect("degraded: a seat is benched");
        assert!(
            why.contains("1 of 3 seats benched: copilot (quota_exhausted (2/4 ballots) — ballot)"),
            "{why}"
        );
        assert_eq!(
            dists[0].benched, dists[1].benched,
            "the bench rides every unit"
        );
    }

    /// (core#461 acceptance b, c) The first-occurrence causes beside a round-2 abstention: a
    /// `not_logged_in` ballot (claude's stdout refusal) and a `not_installed` spawn each bench the
    /// seat by name; the abstention never masks them. The review moves past the seat both times.
    #[test]
    fn a_login_or_missing_binary_ballot_plus_an_abstention_benches_on_first_occurrence() {
        for (failure, reason) in [
            (
                login_refusal as fn() -> wicked_council::types::SeatFailure,
                "not_logged_in",
            ),
            (missing_binary, "not_installed (2/4 ballots)"),
        ] {
            let stub = dead_then_benched("copilot", failure, false);
            let dispatcher: Arc<dyn Dispatcher + Send + Sync> = stub.clone();
            let roster = [seat("claude"), seat("copilot"), seat("codex")];
            let dists = distribute_units_against_benched(
                &build_and_review(),
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
            assert!(
                stub.rounds_seen.load(Ordering::SeqCst) >= 2,
                "{reason}: a runoff ran"
            );
            assert_eq!(
                dists[0].benched.len(),
                1,
                "{reason}: {:?}",
                dists[0].benched
            );
            assert_eq!(dists[0].benched[0].cli, "copilot", "{reason}");
            assert_eq!(dists[0].benched[0].reason, reason);
            assert_eq!(dists[0].benched[0].source, "ballot");
            assert_eq!(
                dists[1].assigned_cli, "codex",
                "{reason}: the review moves past the dead seat: {:?}",
                dists[1].routing
            );
            let why = dists[1].degraded_reason.as_deref().expect("degraded");
            assert!(
                why.contains(&format!("copilot ({reason} — ballot)")),
                "{why}"
            );
        }
    }

    /// (core#461 acceptance d) One VOTE on round 1 and the dispatcher's abstention on round 2 is a
    /// health-gated flaky seat, not a dead one: nothing is benched, `degradedReason` stays `null`,
    /// and the seat is still the reviewer — the abstention alone is not evidence.
    #[test]
    fn a_vote_followed_by_an_abstention_is_not_a_dead_seat() {
        let stub = dead_then_benched("copilot", quota_refusal, true);
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = stub.clone();
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
        let dists = distribute_units_against_benched(
            &build_and_review(),
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
        assert!(
            stub.rounds_seen.load(Ordering::SeqCst) >= 2,
            "the councils ran a runoff, so the abstention happened"
        );
        assert!(
            dists.iter().all(|d| d.benched.is_empty()),
            "a vote keeps the seat: {:?}",
            dists[0].benched
        );
        assert!(dists.iter().all(|d| d.degraded_reason.is_none()));
        assert_eq!(
            dists[1].assigned_cli, "copilot",
            "still the first non-builder seat: {:?}",
            dists[1].routing
        );
    }

    /// (core#461) The ledger's reading of the dispatcher's abstention, one clause per case:
    /// corroboration beside a dead-class ballot, part of the dispatcher's own streak after a
    /// timeout, proof of nothing alone, never a mask over a vote or an unclassified failure.
    #[test]
    fn a_dispatcher_abstention_corroborates_a_dead_ballot_and_proves_nothing_alone() {
        use wicked_council::types::{SeatFailureKind as K, SeatFailureReason as R};
        let quota = Some((K::NonZeroExit, Some(R::QuotaExhausted)));
        let login = Some((K::NonZeroExit, Some(R::NotLoggedIn)));
        let missing = Some((K::SpawnFailed, Some(R::NotInstalled)));
        let unclassified = Some((K::NonZeroExit, None));
        let timeout = Some((K::TimedOut, None));
        let abstained = Some((K::Benched, None));
        let ledger = |records: &[Option<(K, Option<R>)>]| {
            let mut l = SeatLedger::default();
            for r in records {
                l.record(*r);
            }
            l
        };

        assert_eq!(
            ledger(&[abstained, abstained]).dead_seat_reason(2),
            None,
            "abstentions alone are not evidence — the seat was never asked"
        );
        assert_eq!(
            ledger(&[quota, abstained]).dead_seat_reason(2).as_deref(),
            Some("quota_exhausted (1/2 ballots)"),
            "the smoke S04 shape"
        );
        assert_eq!(
            ledger(&[login, abstained]).dead_seat_reason(2).as_deref(),
            Some("not_logged_in")
        );
        assert_eq!(
            ledger(&[missing, abstained]).dead_seat_reason(2).as_deref(),
            Some("not_installed (1/2 ballots)")
        );
        assert_eq!(
            ledger(&[timeout, abstained]).dead_seat_reason(2).as_deref(),
            Some("timed_out (1/2 ballots, no vote returned)"),
            "one timeout then the dispatcher's own bench for it is the streak it counted"
        );
        assert_eq!(
            ledger(&[timeout, abstained]).dead_seat_reason(3),
            None,
            "below the streak threshold"
        );
        assert_eq!(
            ledger(&[None, abstained]).dead_seat_reason(2),
            None,
            "a vote keeps the seat"
        );
        assert_eq!(
            ledger(&[quota, None, abstained]).dead_seat_reason(2),
            None,
            "one refusal beside a vote is a flaky provider"
        );
        assert_eq!(
            ledger(&[quota, unclassified, abstained]).dead_seat_reason(2),
            None,
            "an unclassified failure still proves nothing"
        );
    }

    /// (core#461) `seat_ballots` tallies EVERY round of a council — one ballot per seat per round,
    /// the abstention included — and a status that kept no history falls back to its latest round.
    #[test]
    fn seat_ballots_read_every_round_and_fall_back_to_the_latest_one() {
        use wicked_council::store::SeatFailureRecord;
        use wicked_council::types::{SeatFailureKind as K, SeatFailureReason as R};
        /// One `(cli, failure)` per ballot, in tally order.
        type Tally = Vec<(String, Option<(K, Option<R>)>)>;
        let convened = vec![
            "claude".to_string(),
            "copilot".to_string(),
            "codex".to_string(),
        ];
        let record = |cli: &str, failure: wicked_council::types::SeatFailure| SeatFailureRecord {
            cli: cli.into(),
            failure,
        };
        let benched =
            || wicked_council::types::SeatFailure::new(K::Benched, "seat benched after failures");
        let mut status = status_with_winner(Some("1 — fit"), TaskState::Voted);
        status.seat_failures = vec![record("copilot", benched())];
        status.seat_failure_history = vec![
            vec![record("copilot", quota_refusal())],
            vec![record("copilot", benched())],
        ];
        let ballots: Tally = seat_ballots(&status, &convened)
            .into_iter()
            .map(|b| (b.cli, b.failure))
            .collect();
        assert_eq!(
            ballots,
            vec![
                ("claude".to_string(), None),
                (
                    "copilot".to_string(),
                    Some((K::NonZeroExit, Some(R::QuotaExhausted)))
                ),
                ("codex".to_string(), None),
                ("claude".to_string(), None),
                ("copilot".to_string(), Some((K::Benched, None))),
                ("codex".to_string(), None),
            ],
            "round 1 then round 2, every convened seat on each"
        );
        // No history (a status from a hand-built council): the latest round alone, abstention kept.
        status.seat_failure_history.clear();
        let ballots: Tally = seat_ballots(&status, &convened)
            .into_iter()
            .map(|b| (b.cli, b.failure))
            .collect();
        assert_eq!(
            ballots,
            vec![
                ("claude".to_string(), None),
                ("copilot".to_string(), Some((K::Benched, None))),
                ("codex".to_string(), None),
            ]
        );
        // A council that never ran yields no evidence at all.
        status.state = TaskState::Running;
        assert!(seat_ballots(&status, &convened).is_empty());
    }

    /// (b) A MIXED record — one quota refusal, one vote — is a flaky provider, not a dead seat:
    /// nothing is benched, `degradedReason` stays `null`, and the seat is still the reviewer.
    /// A seat that fails EVERY ballot with words the council cannot classify, on the other hand,
    /// is dead for the run by persistence (R5b / DES-L3 3D′ — the class crew used to carry in
    /// its own cross-run ledger): benched with the unclassified reason, never a guessed one.
    #[test]
    fn a_seat_with_one_quota_refusal_and_one_vote_is_not_benched_but_a_persistent_crash_is() {
        let dispatcher = dead_seat("copilot", quota_refusal, true);
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
        let dists = distribute_units_against_benched(
            &build_and_review(),
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
        assert!(
            dists.iter().all(|d| d.benched.is_empty()),
            "a mixed record is not a dead seat: {:?}",
            dists[0].benched
        );
        assert!(
            dists.iter().all(|d| d.degraded_reason.is_none()),
            "{:?}",
            dists[1].degraded_reason
        );
        assert_eq!(
            dists[1].assigned_cli, "copilot",
            "still the first non-builder seat: {:?}",
            dists[1].routing
        );

        fn crashed() -> wicked_council::types::SeatFailure {
            wicked_council::types::SeatFailure::new(
                wicked_council::types::SeatFailureKind::NonZeroExit,
                "exit 139",
            )
            .with_output("", "segmentation fault")
        }
        let dispatcher = dead_seat("copilot", crashed, false);
        let dists = distribute_units_against_benched(
            &build_and_review(),
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
        let bench = dists[0]
            .benched
            .iter()
            .find(|b| b.cli == "copilot")
            .expect("a seat that crashes on every ballot is dead for the run (3D′)");
        assert!(
            bench.reason.starts_with("non_zero_exit (")
                && bench.reason.ends_with("ballots, unclassified)"),
            "{}",
            bench.reason
        );
        assert_eq!(bench.source, "ballot");
    }

    /// (c) A seat whose binary cannot be spawned (`NotFound`) is benched `not_installed` on its
    /// first ballot — the missing binary does not appear mid-run.
    #[test]
    fn a_seat_whose_ballot_cannot_spawn_is_benched_as_not_installed() {
        fn missing() -> wicked_council::types::SeatFailure {
            wicked_council::types::SeatFailure::spawn_failed(
                "copilot",
                &std::io::Error::new(std::io::ErrorKind::NotFound, "No such file or directory"),
            )
        }
        let dispatcher = dead_seat("copilot", missing, false);
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
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
        let why = dists[0].degraded_reason.as_deref().expect("benched");
        assert!(
            why.contains("1 of 3 seats benched: copilot (not_installed (3/3 ballots) — ballot)"),
            "{why}"
        );
        assert_eq!(dists[0].benched[0].source, "ballot");
    }

    /// (d / AC-3 / core#537, core#560) When a ballot-bench empties the evaluator pool,
    /// distribution FAILS CLOSED: no silent creator_seat fallback. The operator relaunches once
    /// the seat recovers. A bench-free too-small roster is unchanged — that path stays open.
    #[test]
    fn when_a_ballot_bench_empties_the_evaluator_pool_distribution_fails_closed() {
        let dispatcher = dead_seat("copilot", quota_refusal, false);
        let roster = [seat("claude"), seat("copilot")];
        let err = distribute_units_against_benched(
            &build_and_review(),
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect_err("ballot-bench must fail closed — no silent creator_seat fallback");
        let msg = err.to_string();
        assert!(
            msg.contains("evaluator\u{2260}creator unsatisfiable"),
            "error must name the constraint: {msg}"
        );
        assert!(
            msg.contains("distinct seat(s) benched"),
            "error must name benched seats as the cause: {msg}"
        );
        assert!(
            msg.contains("— ballot"),
            "error must name ballot as the source: {msg}"
        );
        // Verify it is the typed NoEligibleSeat error (parks at dead_seat gate, not sessionFailed).
        assert!(
            err.downcast_ref::<crate::NoEligibleSeat>().is_some(),
            "must be NoEligibleSeat for dead_seat gate routing: {err:?}"
        );
    }

    /// (core#560) When a LAUNCHER-bench empties the evaluator pool (the distinct seat was found
    /// unusable by the health probe before ballots ran), distribution fails closed. The error names
    /// the launcher as the source.
    #[test]
    fn when_a_launcher_bench_makes_evaluator_creator_unsatisfiable_distribution_fails_closed() {
        let (dispatcher, _) = spy();
        let mut pi = seat("pi");
        pi.health = Some(wicked_council::types::SeatHealth::unusable(
            "unusable (launcher health probe)",
        ));
        let roster = [seat("claude"), pi];
        let err = distribute_units_against_benched(
            &build_and_review(),
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect_err("launcher-bench must fail closed — no silent creator_seat fallback");
        let msg = err.to_string();
        assert!(
            msg.contains("evaluator\u{2260}creator unsatisfiable"),
            "error must name the constraint: {msg}"
        );
        assert!(
            msg.contains("distinct seat(s) benched"),
            "error must name benched seats as the cause: {msg}"
        );
        assert!(
            msg.contains("— launcher"),
            "error must name launcher as the source: {msg}"
        );
        assert!(
            err.downcast_ref::<crate::NoEligibleSeat>().is_some(),
            "must be NoEligibleSeat: {err:?}"
        );
    }

    /// (core#560) When a WORKER-bench empties the evaluator pool (a prior dispatch's worker
    /// returned an auth refusal, benching the distinct seat before this re-plan), distribution
    /// fails closed. The error names the worker as the source.
    #[test]
    fn when_a_worker_bench_makes_evaluator_creator_unsatisfiable_distribution_fails_closed() {
        let (dispatcher, _) = spy();
        let roster = [seat("claude"), seat("pi")];
        let worker_bench = crate::domain::BenchedSeat {
            cli: "pi".to_string(),
            reason: "auth_refusal (worker transcript)".to_string(),
            source: "worker".to_string(),
        };
        let err = distribute_units_against_benched(
            &build_and_review(),
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[worker_bench],
        )
        .expect_err("worker-bench must fail closed — no silent creator_seat fallback");
        let msg = err.to_string();
        assert!(
            msg.contains("evaluator\u{2260}creator unsatisfiable"),
            "error must name the constraint: {msg}"
        );
        assert!(
            msg.contains("distinct seat(s) benched"),
            "error must name benched seats as the cause: {msg}"
        );
        assert!(
            msg.contains("— worker"),
            "error must name worker as the source: {msg}"
        );
        assert!(
            err.downcast_ref::<crate::NoEligibleSeat>().is_some(),
            "must be NoEligibleSeat: {err:?}"
        );
    }

    /// (AC-3) When BOTH seats are healthy, routing is unchanged — fail-closed only fires on bench.
    #[test]
    fn when_both_seats_are_healthy_evaluator_distinct_routes_normally() {
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(SpyDispatcher {
            calls: Arc::new(AtomicUsize::new(0)),
        });
        let roster = [seat("claude"), seat("copilot")];
        let dists = distribute_units_against_benched(
            &build_and_review(),
            &roster,
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect("both healthy — routing must succeed");
        assert_eq!(dists[0].assigned_cli, "claude", "build stays on claude");
        assert_ne!(
            dists[1].assigned_cli, dists[0].assigned_cli,
            "review must be on a distinct seat: {:?}",
            dists[1].routing
        );
    }

    /// A seat that times out on a STREAK of ballots (the dispatcher's own threshold, default 2)
    /// and votes on none is benched `timed_out`; a single timeout is a slow answer and is not.
    #[test]
    fn a_timeout_streak_with_no_vote_benches_the_seat_and_a_single_timeout_does_not() {
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
        let dispatcher = dead_seat("copilot", timed_out_ballot, false);
        let dists = distribute_units_against_benched(
            &build_and_review(),
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
        let why = dists[0].degraded_reason.as_deref().expect("benched");
        assert!(
            why.contains("copilot (timed_out (6/6 ballots, no vote returned) — ballot)"),
            "{why}"
        );
        assert_eq!(
            dists[1].assigned_cli, "codex",
            "the review moves past it: {:?}",
            dists[1].routing
        );

        // ONE timeout: a council that closes on its first ballot — three live votes of four
        // seated clear the 75 % bar — so the seat is asked exactly once. (core#461: the ledger
        // now counts every ROUND; on the 3-seat roster above two live votes of three are 67 %,
        // the council runs the 3-round cap and a seat timing out on all three IS the streak the
        // dispatcher's own threshold names — the old "1/1" there was the last round alone.)
        let roster = [seat("claude"), seat("copilot"), seat("codex"), seat("pi")];
        let dispatcher = dead_seat("copilot", timed_out_ballot, false);
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
        .expect("routes");
        assert!(dists[0].benched.is_empty(), "{:?}", dists[0].benched);
        assert_eq!(dists[0].degraded_reason, None);
    }

    /// (review F1 on #452; re-baselined for R5b / DES-L3 3D′) A seat whose ballot exits
    /// non-zero after printing a vote ABOUT a rate limiter — subject words, no refusal — is
    /// never CLASSIFIED `quota_exhausted` from those words. It IS benched, but by the
    /// unclassified-PERSISTENT arm (it failed every ballot it was asked and never voted), with
    /// the reason saying exactly that — the count is the evidence, not the words.
    #[test]
    fn a_failed_ballot_whose_words_are_about_rate_limiting_never_classifies_as_quota() {
        fn subject_words() -> wicked_council::types::SeatFailure {
            wicked_council::types::SeatFailure::new(
                wicked_council::types::SeatFailureKind::NonZeroExit,
                "exit 1",
            )
            .with_output(
                "Option 1 — fit. Top risk: the rate limit middleware has no tests.",
                "",
            )
        }
        let dispatcher = dead_seat("copilot", subject_words, false);
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
        let dists = distribute_units_against_benched(
            &build_and_review(),
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
        let bench = dists[0]
            .benched
            .iter()
            .find(|b| b.cli == "copilot")
            .expect("a seat that failed every ballot without a vote is benched (3D′)");
        assert!(
            bench.reason.starts_with("non_zero_exit (")
                && bench.reason.ends_with("ballots, unclassified)"),
            "the words never classify — the count does: {}",
            bench.reason
        );
        assert!(
            !bench.reason.contains("quota"),
            "subject words about a rate limiter must not read as a quota refusal: {}",
            bench.reason
        );
        assert_ne!(
            dists[1].assigned_cli, "copilot",
            "the review unit leaves the benched seat: {:?}",
            dists[1].routing
        );
    }

    /// (review F2 on #452) A bench-free SINGLE-seat roster is unchanged: no bench, `degradedReason`
    /// `null`, the review stays on the only seat — and the evaluator≠creator stderr warning is for
    /// a roster that COULD have separated, never for one seat.
    #[test]
    fn a_single_seat_roster_is_unchanged_and_never_warned_about() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(RecordingDispatcher { seen });
        let dists = distribute_units_against_benched(
            &build_and_review(),
            &[seat("claude")],
            "s1",
            None,
            &dispatcher,
            None,
            None,
            None,
            &[],
        )
        .expect("routes");
        assert!(dists
            .iter()
            .all(|d| d.degraded_reason.is_none() && d.benched.is_empty()));
        assert_eq!(dists[1].assigned_cli, "claude");
        // (core#461) …but the fallback is DISCLOSED as a field: the review stays on the seat that
        // built, and the wire says so without prose (`degradedReason` stays `null` here).
        assert_eq!(
            dists[0].distinctness_fallback, None,
            "the build unit is no evaluator"
        );
        assert_eq!(
            dists[1].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_CREATOR_SEAT)
        );

        let keys = |ks: &[&str]| ks.iter().map(|k| k.to_string()).collect::<Vec<_>>();
        let builders = |ks: &[&str]| {
            ks.iter()
                .map(|k| k.to_string())
                .collect::<std::collections::HashSet<_>>()
        };
        assert!(
            !warns_about_missing_evaluator_seat(&keys(&["claude"]), &builders(&["claude"])),
            "one seat has nothing to separate"
        );
        assert!(warns_about_missing_evaluator_seat(
            &keys(&["claude", "codex"]),
            &builders(&["claude", "codex"])
        ));
        assert!(!warns_about_missing_evaluator_seat(
            &keys(&["claude", "codex"]),
            &builders(&["claude"])
        ));
    }

    /// A unit the council handed to a seat the ledger then benched is reassigned with the CAUSE
    /// in the routing — the F-7R2-006 sentence, generalised past authentication.
    #[test]
    fn a_unit_handed_to_a_quota_dead_seat_is_reassigned_naming_the_cause() {
        /// copilot fails on quota; the voters pick option 2 — which IS copilot.
        struct PickTheDeadSeat;
        impl Dispatcher for PickTheDeadSeat {
            fn dispatch(&self, cli: &AgenticCli, _task: &CouncilTask) -> Option<Vote> {
                Some(Vote {
                    cli: cli.key.clone(),
                    recommendation: "2 — fit".into(),
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
                _ctx: &wicked_council::types::BallotContext,
            ) -> wicked_council::types::DispatchOutcome {
                if cli.key == "copilot" {
                    wicked_council::types::DispatchOutcome::Failed(quota_refusal())
                } else {
                    wicked_council::types::DispatchOutcome::Voted(self.dispatch(cli, task).unwrap())
                }
            }
        }
        let dispatcher: Arc<dyn Dispatcher + Send + Sync> = Arc::new(PickTheDeadSeat);
        let roster = [seat("claude"), seat("copilot"), seat("codex")];
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
        assert_eq!(dists[0].assigned_cli, "claude", "{:?}", dists[0].routing);
        match &dists[0].routing {
            RoutingInfo::Degraded { reason } => assert!(
                reason.contains("council picked 'copilot'")
                    && reason.contains("exhausted its quota on its ballot")
                    && reason.contains("(quota_exhausted (3/3 ballots))")
                    && reason.contains("reassigned to 'claude'"),
                "{reason}"
            ),
            other => panic!("expected a degraded routing, got {other:?}"),
        }
    }
}
