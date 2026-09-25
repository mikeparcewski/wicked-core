//! DISTRIBUTE — assign each planned unit a seat, deterministically (core#590 S5).
//!
//! Distribution convenes NO council. It used to convene one per unit to vote on a seat; that cost
//! a council of heavy CLI subprocesses per phase whether or not anything was in question, and it
//! degraded silently when seats benched (core#590, run `aee254f1`: twelve failed ballots, no
//! contribution to any verdict). A council is now summoned only for a concrete, disputed decision
//! ([`crate::decision`]). Routing here is the engine's existing deterministic path:
//!
//! * a Tool unit runs the engine's own command (`RoutingInfo::Tool`), as before;
//! * a seated unit takes the FIRST of its candidate seats in roster order — the eligible seats
//!   its skills admit ([`seat_candidates`], core#401) — recorded `RoutingInfo::Teamed`. That is
//!   the seat the council path itself fell back to when it could not agree;
//! * the evaluator≠creator fence ([`enforce_evaluator_distinct`]) then runs unchanged: a
//!   review/test unit on a builder seat moves to a seat that did not build
//!   (`RoutingInfo::EvaluatorDistinct`), is disclosed when none exists on a bench-free roster, and
//!   is REFUSED when a bench is what emptied the pool.
//!
//! The ONE thing distribution refuses — at plan time, before any unit runs — is a roster with no
//! eligible seat for a unit ([`crate::skills_snapshot::SkillsError::NoEligibleSeat`],
//! [`crate::NoEligibleSeat`]).

use wicked_council::AgenticCli;

use crate::domain::{BenchedSeat, RoutingInfo, WorkUnit};
use crate::skills_snapshot::{SeatRequirement, SkillsError, SkillsSnapshot, NONPORTABLE_SEAT};

/// The distribution decision for one unit (positionally aligned with the input units).
#[derive(Debug, Clone)]
pub struct Distribution {
    pub assigned_cli: String,
    /// The assigned CLI's invocation template (so the runner can execute an ad-hoc CLI not in the
    /// registry). Resolved from the launch roster.
    pub assigned_invocation: Option<String>,
    /// WHY this seat — `teamed` (the deterministic pick), `evaluator_distinct` (the fence moved
    /// it) or `tool` — made visible for the UI.
    pub routing: RoutingInfo,
    /// WHY the candidate seats were narrowed before routing (core#401) — the unit's
    /// skills admit only a claude seat — or `None` when every roster seat was a candidate. Rides
    /// [`CoreEvent::UnitDistributed`]`.seat_constraint`; `routing` reads exactly as before.
    pub seat_constraint: Option<String>,
    /// (F-7R2-006, wave 6) WHY the seats this unit was routed among were FEWER than the roster
    /// the launcher configured — `"N of M seats benched: codex (signed out — launcher), pi
    /// (not_logged_in — worker)"`. `None` when every configured seat was eligible and for a tool
    /// unit. Rides `unitDistributed.degradedReason` for every seated routing method (crew#533's
    /// anchor).
    pub degraded_reason: Option<String>,
    /// (F-7R2-006) The run-level bench set this distribution ran under — the launcher's unusable
    /// seats plus the session's persisted bench (a worker's auth refusal, a recorded run's ballot
    /// bench) — identical on every unit of one
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
    /// else runs, so an empty bench means no seat was benched by a worker, a recorded ballot OR the
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
/// key), no invocation, routing `tool`. The caller sets `benched` when a bench rides.
fn tool_distribution(unit: &WorkUnit) -> Distribution {
    Distribution {
        assigned_cli: unit
            .tool_cmd
            .as_ref()
            .and_then(|c| c.first())
            .cloned()
            .unwrap_or_else(|| "__tool__".to_string()),
        assigned_invocation: None,
        routing: RoutingInfo::Tool,
        seat_constraint: None,
        degraded_reason: None,
        benched: Vec::new(),
        distinctness_fallback: None,
    }
}

/// (core#590 S5) The distribution of a seated unit: its FIRST candidate seat, in roster order —
/// no council, no ballot. `candidates` is never empty here: an empty eligible set and a unit no
/// seat admits are both refused before any unit is routed.
fn teamed_distribution(candidates: &[AgenticCli]) -> Distribution {
    let winner = candidates
        .first()
        .map(|c| c.key.clone())
        .expect("routing is reached only with an eligible candidate seat");
    Distribution {
        assigned_invocation: invocation_of(candidates, &winner),
        assigned_cli: winner.clone(),
        routing: RoutingInfo::Teamed { winner },
        seat_constraint: None,
        degraded_reason: None,
        benched: Vec::new(),
        distinctness_fallback: None,
    }
}

/// Route every unit to a seat (deterministic — no council; see the module docs).
pub fn distribute_units_on(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
) -> anyhow::Result<Vec<Distribution>> {
    distribute_units_on_benched(units, clis, session_id, &[])
}

/// [`distribute_units_on`] for a run that already BENCHED seats (F-7R2-006): `prior_benched` —
/// the session's persisted bench set (a resume, a re-plan) — is honoured beside the roster's
/// own health verdicts; a benched seat is never routed to.
pub fn distribute_units_on_benched(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
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
    distribute_units_against_benched(units, clis, session_id, snapshot.as_ref(), prior_benched)
}

/// The candidate seats for one unit: the roster records routing picks among, and WHY they were
/// narrowed — or `None` when every roster seat is a candidate.
type Candidates = Option<(Vec<AgenticCli>, String)>;

/// [`distribute_units_on`] against an explicit skills root (`None` ⇒ no seat is constrained), so
/// the routing is testable without the process environment — the same split the admission has
/// (`resolve_ladder` / `resolve_ladder_in`).
#[cfg(test)]
pub(crate) fn distribute_units_against(
    units: &[WorkUnit],
    clis: &[AgenticCli],
    session_id: &str,
    snapshot: Option<&SkillsSnapshot>,
) -> anyhow::Result<Vec<Distribution>> {
    distribute_units_against_benched(units, clis, session_id, snapshot, &[])
}

/// The seats of `clis` a run may still route to: not in `benched`, and not declared unusable by
/// the launcher's health probe ([`AgenticCli::health`]). Pure; the ONE eligibility rule routing,
/// the evaluator≠creator reassignment, the failover ladder and the judge/triage seat selection
/// all read (F-7R2-006).
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
/// behind it (`model_of`): `seat_is_claude`, and the `same_cli_instance` disclosure in rule 3.
///
/// 0. SEAT REQUIREMENT is per UNIT (F-E2E-011) — a Tool-executor unit (`tool_cmd`) is the engine's
///    own command and is handed to no seat. A plan whose EVERY unit is a tool therefore needs no
///    seat at all and is routed `tool` before any eligibility verdict — crew launches such a run
///    (`onboarding`: index + annotate) with `clis: []` by design (wicked-crew#533). Rules 1–4
///    apply once at least one planned unit needs a seat.
/// 1. ELIGIBILITY — the seats the launcher declared unusable (`health.usable == false`) and every
///    seat in `prior_benched` are set aside. An empty eligible set REFUSES the plan by name —
///    better than five human gates on dead seats.
/// 2. PICK (core#590 S5) — each seated unit takes the first of its candidate seats in roster order
///    (the eligible seats its skills admit, core#401): `RoutingInfo::Teamed`. No council, no
///    ballot, so no seat is benched here.
/// 3. EVALUATOR ≠ CREATOR — [`enforce_evaluator_distinct`] moves a review/test unit off a builder
///    seat onto a still-eligible seat its skills admit (`RoutingInfo::EvaluatorDistinct`). When a
///    BENCH leaves a review/test unit no seat distinct from its creator the plan is REFUSED
///    (`NoEligibleSeat`, core#560). A BENCH-FREE roster with no seat distinct from the builders
///    instead keeps the review/test unit on its creator seat: a one-seat roster, a roster whose
///    every seat was assigned a Build/Recon unit, or one whose only non-builder seats this unit's
///    skills refuse. That case is disclosed by the `distinctness_fallback: "creator_seat"` field
///    (core#461), not by `degraded_reason`; only the all-seats-built shape ALSO warns on stderr
///    (a one-seat roster never does — it had nothing to separate). A review/test unit that DID get
///    a seat distinct from every builder seat, but one running the same CLI (`claude#2` grading
///    `claude#1` — two seat INSTANCES of one cli, core#591), is disclosed by the same field as
///    `"same_cli_instance"`: instance-distinct, not model-distinct.
/// 4. `degraded_reason` names the bench on EVERY unit whenever eligible < configured, and the
///    whole bench rides each `Distribution` for the actor to persist.
pub(crate) fn distribute_units_against_benched(
    units: &[WorkUnit],
    configured: &[AgenticCli],
    session_id: &str,
    snapshot: Option<&SkillsSnapshot>,
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
    // (F-E2E-011, rule 0) No planned unit needs a seat ⇒ nothing to pick and nothing to refuse:
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
             seat in, or add one; routing to dead seats would only park the run at a human \
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
            "wicked-core: distribution for {session_id} routes among {} of {} configured seats — {}",
            eligible.len(),
            configured.len(),
            crate::domain::benched_summary(&benched, configured.len()).unwrap_or_default()
        );
    }
    let clis: &[AgenticCli] = &eligible;
    // Plan-time refusal (core#401): a unit whose skills only a claude seat can be handed, on a
    // roster with none, is refused HERE — before any unit does work — naming the skill, its
    // portability and the seat kind required. The ladder would have refused the same unit by name
    // at launch; by then work may have been done and the escalation gate cannot retarget a seat,
    // so the run could only be cancelled.
    let candidates = seat_candidates(units, clis, snapshot)?;
    // (core#590 S5, rule 2) The pick: deterministic, no council.
    let mut dists: Vec<Distribution> = units
        .iter()
        .zip(candidates.iter())
        .map(|(unit, candidates)| {
            if unit.tool_cmd.is_some() {
                return tool_distribution(unit);
            }
            match candidates {
                Some((admitted, why)) => Distribution {
                    seat_constraint: Some(why.clone()),
                    ..teamed_distribution(admitted)
                },
                None => teamed_distribution(clis),
            }
        })
        .collect();
    let still_eligible: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    // (DES-TEAMING-002 §8.1, seam D1) A TEAM RUN — units of the run's composed per-run def,
    // stamped `team_run` at plan time — never grades on its creator seat and prefers a distinct
    // CLI over a second instance of the creator's.
    let team_run = units.iter().any(|u| u.team_run);
    let (same_seat, same_cli_instance) = enforce_evaluator_distinct(
        units,
        &mut dists,
        &still_eligible,
        clis,
        &candidates,
        team_run,
    );
    // (AC-3 / core#537, core#560) When a benched seat — from ANY source (launcher health probe,
    // or a worker transcript persisted on the session) — made evaluator≠creator unsatisfiable,
    // fail CLOSED: the operator must relaunch once the seat recovers. A silent creator_seat
    // fallback lets a compromised or broken seat evaluate its own work — the fence exists to
    // prevent that. A bench-free roster that is simply too small is unchanged: when benched is
    // empty, the condition is false and the pre-existing creator_seat fallback applies.
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
    // (DES-TEAMING-002 §8.1, seam D1) A team run refuses the creator-seat fallback even on a
    // BENCH-FREE roster: no distinct CLI and no usable second instance ⇒ `NoEligibleSeat`, naming
    // the units and the instance that would satisfy them. The engine never mints or signs in an
    // instance; the launcher adds the ones it has configured to the roster.
    if !same_seat.is_empty() && team_run {
        // Reached only BENCH-FREE (the bench arm above returned otherwise), so every configured
        // seat is usable: an instance the roster already holds is a builder or refused by the
        // unit's skills, and the remedy is a FRESH key. An unusable configured instance is named
        // by the bench arm instead (codex round 5 on #618).
        debug_assert!(benched.is_empty());
        let blocked: Vec<&WorkUnit> = units
            .iter()
            .filter(|u| u.tool_cmd.is_none() && same_seat.contains(&u.ord))
            .collect();
        let ords: Vec<u32> = blocked.iter().map(|u| u.ord).collect();
        let mut missing: Vec<String> = Vec::new();
        for (u, d) in units.iter().zip(dists.iter()) {
            if blocked.iter().any(|b| b.ord == u.ord) {
                let key = next_instance_key(configured, model_of(&d.assigned_cli));
                if !missing.contains(&key) {
                    missing.push(key);
                }
            }
        }
        return Err(crate::NoEligibleSeat {
            run_id: session_id.to_string(),
            benched: format!(
                "evaluator\u{2260}creator unsatisfiable for unit(s) {ords:?}: team run \u{2014} no \
                 seat distinct from the creator and no usable second instance (add a signed-in \
                 {} to the roster); a team run never grades on its creator seat",
                missing.join(" or ")
            ),
            benched_seats: benched.clone(),
        }
        .into());
    }
    // (F-7R2-006 rule 4) `degradedReason` on EVERY unit whenever eligible < configured. The
    // evaluator≠creator fallback is a FIELD (core#461, core#591): `creator_seat` for a
    // review/test unit left on a seat that built what it checks (necessarily on a BENCH-FREE
    // roster — every bench-induced distinctness failure is refused above), `same_cli_instance`
    // for one on a distinct seat instance of a builder's own cli. `creator_seat` dominates;
    // `enforce_evaluator_distinct` returns the two sets already disjoint.
    let summary = crate::domain::benched_summary(&benched, configured.len());
    for (u, d) in units.iter().zip(dists.iter_mut()) {
        d.benched = benched.clone();
        d.distinctness_fallback = if u.tool_cmd.is_some() {
            None
        } else if same_seat.contains(&u.ord) {
            Some(DISTINCTNESS_FALLBACK_CREATOR_SEAT.to_string())
        } else if same_cli_instance.contains(&u.ord) {
            Some(DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE.to_string())
        } else {
            None
        };
        d.degraded_reason = match &d.routing {
            RoutingInfo::Tool => None,
            _ => summary.clone(),
        };
    }
    Ok(dists)
}

/// The first seat-instance key of `cli` (`<cli>#2`, `<cli>#3`, …) the roster does not already
/// hold: the instance a refused team run names as its remedy (DES-TEAMING-002 §8.1).
fn next_instance_key(configured: &[AgenticCli], cli: &str) -> String {
    (2u32..)
        .map(|n| format!("{cli}#{n}"))
        .find(|k| !configured.iter().any(|c| &c.key == k))
        .expect("an unbounded range always yields a free key")
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
/// it checks, so after distribution we reassign any review/test unit whose routed CLI matches
/// a build/recon CLI to a roster seat NOT used for building (when the roster has the seats to do so)
/// — a seat the unit's skills ADMIT (core#401): a Claude-only review unit is never moved onto a seat
/// the ladder would refuse it on; with no such alternative it stays where routing put it.
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
    // (DES-TEAMING-002 §8.1) A team run takes a MODEL-distinct seat first and a second instance
    // of a builder's cli only when no model-distinct seat admits the unit. `false` keeps today's
    // roster-order pick.
    prefer_model_distinct: bool,
) -> (Vec<u32>, Vec<u32>) {
    use crate::domain::StageKind;
    let mut same_seat: Vec<u32> = Vec::new();
    let builder_clis: std::collections::HashSet<String> = units
        .iter()
        .zip(dists.iter())
        // A TOOL unit is no creator seat: its `assigned_cli` is its own program token
        // (`tool_distribution`), which may spell a seat key (`claude --version`) — D1 review.
        .filter(|(u, _)| {
            u.tool_cmd.is_none() && matches!(u.stage, StageKind::Build | StageKind::Recon)
        })
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
            let distinct = |k: &&String| !builder_clis.contains(*k) && admits(k);
            let model_distinct = prefer_model_distinct
                .then(|| {
                    roster_keys
                        .iter()
                        .filter(distinct)
                        .find(|k| !builder_models.contains(model_of(k)))
                })
                .flatten();
            match model_distinct.or_else(|| roster_keys.iter().find(distinct)) {
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
    // unit actually landed, whether routing put it there or the reassignment above did. A unit
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

#[cfg(test)]
mod tests {
    use super::*;
    use wicked_council::types::{Category, Confidence, InputMode};

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

    /// A seat whose INVOCATION execs `claude` — the carrier identity the runners judge.
    fn claude_seat(key: &str) -> AgenticCli {
        let mut c = seat(key);
        c.binary = "claude".into();
        c.headless_invocation = "claude -p {PROMPT}".into();
        c.trust_flags = vec!["--dangerously-skip-permissions".into()];
        c
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
    /// (`portable: false`) was routed to copilot, which the ladder then refused by name. Now the
    /// unit's candidates are the claude seats — one here — so it lands on claude with the
    /// constraint named. A unit whose skill IS portable routes over the WHOLE roster (its first
    /// seat, core#590 S5) and no constraint is recorded. Mutation: drop the narrowing in
    /// `seat_candidates` and the first unit lands on copilot (the roster's first seat).
    #[test]
    fn a_nonportable_skill_ref_is_seated_on_claude_while_a_portable_one_routes_over_the_whole_roster(
    ) {
        let (_home, _env, _) = hermetic_home("route-nonportable-home");
        let snapshot = published("route-nonportable");
        let roster = [
            seat_running("copilot", "copilot"),
            seat_running("claude", "claude"),
            seat_running("pi", "pi"),
        ];

        // Non-portable ⇒ claude, never the roster's first seat.
        let dists = distribute_units_against(
            &[skilled(1, "wicked-garden-repo-learn")],
            &roster,
            "s1",
            Some(&snapshot),
        )
        .expect("a claude seat is on the roster: no refusal");
        assert_eq!(dists.len(), 1);
        assert_eq!(dists[0].assigned_cli, "claude", "{:?}", dists[0].routing);
        assert_eq!(
            dists[0].assigned_invocation.as_deref(),
            Some("claude -p {PROMPT}")
        );
        assert_eq!(
            dists[0].routing,
            RoutingInfo::Teamed {
                winner: "claude".into()
            }
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

        // Portable ⇒ unconstrained: the whole roster is the candidate set, no constraint.
        let dists = distribute_units_against(
            &[skilled(2, "wicked-garden-search")],
            &roster,
            "s1",
            Some(&snapshot),
        )
        .expect("portable skill: routed as before");
        assert!(
            dists[0].seat_constraint.is_none(),
            "{:?}",
            dists[0].seat_constraint
        );
        assert_eq!(
            dists[0].assigned_cli, "copilot",
            "the first seat of the FULL roster"
        );
    }

    /// core#468: the run's BASE skill is EXISTENCE-only — it never joins the seat requirement, so a
    /// `portable: false` base skill neither narrows the candidates onto claude nor records a
    /// constraint: the unit routes over the WHOLE roster exactly as for a skill-free unit,
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
        let dists =
            distribute_units_against(&[bare, with_phase_skill], &roster, "s1", Some(&snapshot))
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
                "unit {ord}: the first seat of the FULL roster"
            );
        }
    }

    /// A Claude-less roster cannot seat a non-portable skill anywhere, so the run is refused at
    /// DISTRIBUTION — plan-wide, before any unit ran (the skill-free first unit included), with no
    /// seat assigned — naming the skill, its portability, the seat kind required and the roster
    /// that lacks it. Before: routing seated it, the ladder refused it mid-run, and the
    /// escalation gate could only re-dispatch to the same seat or cancel.
    #[test]
    fn a_claude_less_roster_is_refused_at_plan_time_naming_skill_portability_and_seat_kind() {
        let (_home, _env, _) = hermetic_home("route-refuse-home");
        let snapshot = published("route-refuse");
        let roster = [seat_running("copilot", "copilot"), seat_running("pi", "pi")];
        let mut first = WorkUnit::pending("u1", "s1", 1, "Recon: read the repo");
        first.skill_ref = None;
        let err = distribute_units_against(
            &[first, skilled(2, "wicked-garden-repo-learn")],
            &roster,
            "s1",
            Some(&snapshot),
        )
        .expect_err("no claude seat on the roster");
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
    /// the routing seats a skill-bearing unit on claude before routing can pick otherwise,
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
        let dists = distribute_units_against(
            &[skilled(1, "wicked-garden-search")],
            &[seat_running("pi", "pi"), seat_running("claude", "claude")],
            "s1",
            Some(&snapshot),
        )
        .expect("a claude seat is on the roster");
        assert_eq!(dists[0].assigned_cli, "claude");
        let why = dists[0].seat_constraint.as_deref().expect("constrained");
        assert!(
            why.contains("live plugin cache") && why.contains("wicked-garden-search"),
            "{why}"
        );

        let err = distribute_units_against(
            &[skilled(1, "wicked-garden-search")],
            &[seat_running("pi", "pi")],
            "s1",
            Some(&snapshot),
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
        let mut tool = skilled(2, "wicked-garden-repo-learn");
        tool.tool_cmd = Some(vec!["echo".into(), "hi".into()]);
        let dists = distribute_units_against(
            &[skilled(1, "wicked-garden-repo-learn"), tool],
            &[seat_running("pi", "pi"), seat_running("claude", "claude")],
            "s1",
            None,
        )
        .expect("no root ⇒ no routing-time refusal");
        assert!(
            dists.iter().all(|d| d.seat_constraint.is_none()),
            "{dists:?}"
        );
        assert_eq!(
            dists[0].assigned_cli, "pi",
            "the first seat of the whole roster"
        );
        assert!(matches!(dists[1].routing, RoutingInfo::Tool));

        // A tool unit is skipped even WITH a root that would constrain an agent unit.
        let snapshot = published("route-tool");
        let mut tool = skilled(1, "wicked-garden-repo-learn");
        tool.tool_cmd = Some(vec!["echo".into()]);
        let dists =
            distribute_units_against(&[tool], &[seat_running("pi", "pi")], "s1", Some(&snapshot))
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

    /// Evaluator ≠ creator must not undo the narrowing: a Claude-only REVIEW unit whose routed
    /// seat is the builder's seat is NOT moved onto a seat that cannot take it (it stays put,
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
        let dists = distribute_units_against(
            &[build, review_nonportable, review_portable],
            &roster,
            "s1",
            Some(&snapshot),
        )
        .expect("distributed");
        // The first seat routes everywhere: the builder is claude.
        assert_eq!(dists[0].assigned_cli, "claude");
        // Claude-only review: claude is the builder, pi cannot take the skill ⇒ stays on claude,
        // routing untouched (no EvaluatorDistinct claim for a move that did not happen).
        assert_eq!(dists[1].assigned_cli, "claude");
        assert_eq!(
            dists[1].routing,
            RoutingInfo::Teamed {
                winner: "claude".into()
            }
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
        // plan time, before any unit is routed, instead of seated and refused mid-run.
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
            Some(&snapshot),
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
            Some(&snapshot),
        )
        .expect("the overridden `codex` seat IS a claude seat");
        assert_eq!(dists[0].assigned_cli, "codex");
        assert!(dists[0].seat_constraint.is_some());
        // Its launch resolves the registry template — no roster template was carried.
        assert_eq!(dists[0].assigned_invocation, None);
    }

    /// F-7R2-006 (wave 6): a seat the launcher's health probe found unusable is BENCHED — never
    /// routed to — and `degradedReason` names it on the routed unit.
    #[test]
    fn a_launcher_benched_seat_is_never_routed_to_and_degraded_reason_names_it() {
        let mut signed_out = seat("codex");
        signed_out.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        let roster = [seat("claude"), signed_out, seat("pi")];
        let unit = WorkUnit::pending("u1", "s1", 1, "Build the thing");
        let dists = distribute_units_against_benched(&[unit], &roster, "s1", None, &[])
            .expect("two eligible seats route");
        assert_eq!(dists[0].assigned_cli, "claude");
        assert_eq!(
            dists[0].routing,
            RoutingInfo::Teamed {
                winner: "claude".into()
            }
        );
        assert_eq!(
            dists[0].degraded_reason.as_deref(),
            Some("1 of 3 seats benched: codex (signed out — launcher)")
        );
        assert_eq!(dists[0].benched.len(), 1);
        assert_eq!(dists[0].benched[0].source, "launcher");
    }

    /// Every configured seat benched ⇒ the plan is REFUSED by name, before any unit is routed.
    #[test]
    fn an_all_benched_roster_is_refused_by_name() {
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
    }

    // ── F-E2E-011: the seat requirement is per UNIT — a tool-only plan needs no seat ──────────

    /// A Tool-executor unit in the seeded onboarding shape (`wicked-estate <verb> …`).
    fn tool_unit(ord: u32, verb: &str) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("u{ord}"), "s1", ord, format!("{verb} the repo"));
        u.tool_cmd = Some(vec!["wicked-estate".into(), verb.into()]);
        u
    }

    /// crew hands a tool-only workflow (`onboarding`) `clis: []` (wicked-crew#533): every unit
    /// routes `tool` to the tool's own program and nothing is refused —
    /// core-ts 0.7.22 bailed "every configured seat is benched" here, 1 s into every onboarding.
    #[test]
    fn a_tool_only_plan_needs_no_seat_and_routes_tool_on_an_empty_roster() {
        let dists = distribute_units_against_benched(
            &[tool_unit(1, "index"), tool_unit(2, "clusters")],
            &[],
            "s1",
            None,
            &[],
        )
        .expect("a plan that seats nobody is not refused for having no seat");
        assert_eq!(dists.len(), 2);
        for d in &dists {
            assert_eq!(d.assigned_cli, "wicked-estate");
            assert!(matches!(d.routing, RoutingInfo::Tool), "{:?}", d.routing);
            assert_eq!(d.assigned_invocation, None);
            assert_eq!(d.degraded_reason, None);
            assert!(d.benched.is_empty());
        }
    }

    /// A tool-only plan on a roster whose EVERY seat is benched proceeds too — the seats are
    /// irrelevant to it — and the whole bench (prior + launcher) rides each distribution for the
    /// actor to persist, exactly as it would for a seated plan.
    #[test]
    fn a_tool_only_plan_on_an_all_benched_roster_routes_tool_and_carries_the_bench() {
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
    }

    /// The refusal is unchanged the moment a unit NEEDS a seat: a tool unit beside an agent unit
    /// on an empty roster is refused by the existing message, before anything is routed.
    #[test]
    fn a_mixed_plan_with_an_agent_unit_and_no_seat_is_still_refused() {
        let err = distribute_units_against_benched(
            &[
                tool_unit(1, "index"),
                WorkUnit::pending("u2", "s1", 2, "Build the thing"),
            ],
            &[],
            "s1",
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
    }

    /// (F-7R2-006 unchanged) …and on a roster whose every seat is benched, the refusal still
    /// names the benched seats — the tool unit does not lend the agent unit a seat.
    #[test]
    fn a_mixed_plan_on_an_all_benched_roster_is_still_refused_by_name() {
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
    }

    /// The evaluator≠creator reassignment picks only among still-eligible seats — a benched
    /// seat is skipped even when it is the first non-builder on the roster.
    #[test]
    fn evaluator_distinct_never_moves_a_review_unit_onto_a_benched_seat() {
        let mut benched = seat("b");
        benched.health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        let roster = [seat("a"), benched, seat("c")];
        let build = WorkUnit::pending("u1", "s1", 1, "Build the thing");
        let mut review = WorkUnit::pending("u2", "s1", 2, "Review the thing");
        review.stage = crate::domain::StageKind::Review;
        let dists = distribute_units_against_benched(&[build, review], &roster, "s1", None, &[])
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

    // ── core#591: seat-instance identity ─────────────────────────────────────────────────────

    /// A routed distribution on `key`, for the routing passes that take `dists` directly.
    fn routed_dist(key: &str, invocation: &str) -> Distribution {
        Distribution {
            assigned_cli: key.into(),
            assigned_invocation: Some(invocation.into()),
            routing: RoutingInfo::Teamed { winner: key.into() },
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
            routed_dist("claude", "claude -p {PROMPT}"),
            routed_dist("claude", "claude -p {PROMPT}"),
        ];
        let clis = [seat("claude"), seat("claude#2")];
        let roster_keys = vec!["claude".to_string(), "claude#2".to_string()];
        let (same, same_cli) = enforce_evaluator_distinct(
            &units,
            &mut dists,
            &roster_keys,
            &clis,
            &[None, None],
            false,
        );
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
        for units in [build_and_review().to_vec(), vec![tool_unit(1, "index")]] {
            let err =
                distribute_units_against_benched(&units, &[a.clone(), b.clone()], "s1", None, &[])
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
        let fallback_of = |roster: &[AgenticCli]| -> (Option<String>, String) {
            let dists =
                distribute_units_against_benched(&build_and_review(), roster, "s1", None, &[])
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
        let dists = distribute_units_against_benched(
            &build_and_review(),
            &[seat("claude")],
            "s1",
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

    /// (core#560) When a LAUNCHER-bench empties the evaluator pool (the distinct seat was found
    /// unusable by the health probe before routing), distribution fails closed. The error names
    /// the launcher as the source.
    #[test]
    fn when_a_launcher_bench_makes_evaluator_creator_unsatisfiable_distribution_fails_closed() {
        let mut pi = seat("pi");
        pi.health = Some(wicked_council::types::SeatHealth::unusable(
            "unusable (launcher health probe)",
        ));
        let roster = [seat("claude"), pi];
        let err = distribute_units_against_benched(&build_and_review(), &roster, "s1", None, &[])
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
        let roster = [seat("claude"), seat("copilot")];
        let dists = distribute_units_against_benched(&build_and_review(), &roster, "s1", None, &[])
            .expect("both healthy — routing must succeed");
        assert_eq!(dists[0].assigned_cli, "claude", "build stays on claude");
        assert_ne!(
            dists[1].assigned_cli, dists[0].assigned_cli,
            "review must be on a distinct seat: {:?}",
            dists[1].routing
        );
    }

    /// (review F2 on #452) A bench-free SINGLE-seat roster is unchanged: no bench, `degradedReason`
    /// `null`, the review stays on the only seat — and the evaluator≠creator stderr warning is for
    /// a roster that COULD have separated, never for one seat.
    #[test]
    fn a_single_seat_roster_is_unchanged_and_never_warned_about() {
        let dists = distribute_units_against_benched(
            &build_and_review(),
            &[seat("claude")],
            "s1",
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

    // ── core#590 S5: routing convenes no council ──────────────────────────────────────────────

    fn staged(ord: u32, stage: crate::domain::StageKind) -> WorkUnit {
        let mut u = WorkUnit::pending(format!("u{ord}"), "s1", ord, "Do the thing");
        u.stage = stage;
        u
    }

    /// core#590 S5 — two builders, a review and a test unit on a three-seat roster: every builder
    /// records `Teamed` on the first seat, and evaluator ≠ creator moves the review AND the test
    /// unit off it, exactly as it did when a council routed. Fixed expected values, the rule written out.
    #[test]
    fn routing_is_teamed_and_the_evaluator_fence_still_holds() {
        use crate::domain::StageKind;
        let units = [
            staged(1, StageKind::Build),
            staged(2, StageKind::Build),
            staged(3, StageKind::Review),
            staged(4, StageKind::Test),
        ];
        let dists =
            distribute_units_against(&units, &[seat("a"), seat("b"), seat("c")], "s1", None)
                .expect("routes");
        let seats: Vec<&str> = dists.iter().map(|d| d.assigned_cli.as_str()).collect();
        assert_eq!(seats, ["a", "a", "b", "b"]);
        let teamed_a = RoutingInfo::Teamed { winner: "a".into() };
        let moved = RoutingInfo::EvaluatorDistinct {
            winner: "b".into(),
            was: "a".into(),
        };
        let routings: Vec<RoutingInfo> = dists.iter().map(|d| d.routing.clone()).collect();
        assert_eq!(routings, [teamed_a.clone(), teamed_a, moved.clone(), moved]);
        let invocations: Vec<Option<&str>> = dists
            .iter()
            .map(|d| d.assigned_invocation.as_deref())
            .collect();
        assert_eq!(
            invocations,
            [
                Some("run-a {PROMPT}"),
                Some("run-a {PROMPT}"),
                Some("run-b {PROMPT}"),
                Some("run-b {PROMPT}")
            ]
        );
        for d in &dists {
            assert_eq!(d.distinctness_fallback, None);
            assert_eq!(d.degraded_reason, None);
            assert_eq!(d.seat_constraint, None);
        }
        // The fence's own invariant, stated independently of the routing labels: no review/test
        // unit sits on a seat that built.
        let builders: Vec<&str> = seats[..2].to_vec();
        assert!(seats[2..].iter().all(|s| !builders.contains(s)));
    }

    /// core#590 S5 — on a ONE-seat roster the review has nowhere distinct to go: it stays on the
    /// builder's seat, records `Teamed`, and is DISCLOSED (`creator_seat`) — never silently.
    #[test]
    fn a_one_seat_roster_routes_teamed_and_discloses_the_creator_seat() {
        let dists = distribute_units_against(&build_and_review(), &[seat("solo")], "s1", None)
            .expect("routes");
        let solo = RoutingInfo::Teamed {
            winner: "solo".into(),
        };
        assert_eq!(dists[0].routing, solo);
        assert_eq!(dists[1].routing, solo);
        assert_eq!(dists[0].distinctness_fallback, None);
        assert_eq!(
            dists[1].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_CREATOR_SEAT)
        );
    }

    /// core#590 S5 — a bench that empties the evaluator pool still REFUSES the plan (core#560):
    /// the typed `NoEligibleSeat` naming the blocked unit, never a creator-seat review.
    #[test]
    fn a_bench_that_empties_the_evaluator_pool_still_refuses_the_plan() {
        let mut b = seat("b");
        b.health = Some(wicked_council::types::SeatHealth::unusable(
            "unusable (launcher health probe)",
        ));
        let err = distribute_units_against(&build_and_review(), &[seat("a"), b], "s1", None)
            .expect_err("no distinct evaluator ⇒ refused");
        assert!(
            err.downcast_ref::<crate::NoEligibleSeat>().is_some(),
            "{err:?}"
        );
        assert!(
            err.to_string()
                .contains("evaluator\u{2260}creator unsatisfiable for unit(s) [2]"),
            "{err}"
        );
    }
    // ── DES-TEAMING-002 D1: a team run never grades on its creator seat ─────────────────────

    /// `build_and_review` as a TEAM RUN (units of the run's composed def, `team_run` stamped).
    fn team_build_and_review() -> [WorkUnit; 2] {
        let mut units = build_and_review();
        for u in &mut units {
            u.team_run = true;
        }
        units
    }

    fn unusable(mut c: AgenticCli) -> AgenticCli {
        c.health = Some(wicked_council::types::SeatHealth::unusable(
            "not signed in (launcher health probe)",
        ));
        c
    }

    /// D1 (a): roster `[claude]` plus a usable `claude#2` — the review lands on `claude#2`,
    /// disclosed `same_cli_instance`.
    #[test]
    fn d1_a_team_review_moves_to_a_usable_second_instance() {
        let dists = distribute_units_against_benched(
            &team_build_and_review(),
            &[seat("claude"), seat("claude#2")],
            "r1",
            None,
            &[],
        )
        .expect("a usable second instance satisfies a team run");
        assert_eq!(dists[0].assigned_cli, "claude");
        assert_eq!(dists[0].distinctness_fallback, None);
        assert_eq!(dists[1].assigned_cli, "claude#2");
        assert_eq!(
            dists[1].routing,
            RoutingInfo::EvaluatorDistinct {
                winner: "claude#2".into(),
                was: "claude".into()
            }
        );
        assert_eq!(
            dists[1].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE)
        );
    }

    /// D1 (b): roster `[claude]`, bench-free — refused `NoEligibleSeat`, naming the unit and the
    /// missing instance. Never `creator_seat`.
    #[test]
    fn d1_b_team_run_without_a_second_instance_is_refused_bench_free() {
        let err = distribute_units_against_benched(
            &team_build_and_review(),
            &[seat("claude")],
            "r1",
            None,
            &[],
        )
        .expect_err("a team run never falls back to the creator seat");
        let refusal = err
            .downcast_ref::<crate::NoEligibleSeat>()
            .unwrap_or_else(|| panic!("must be NoEligibleSeat: {err:?}"));
        assert_eq!(refusal.run_id, "r1");
        assert!(refusal.benched_seats.is_empty(), "no bench caused it");
        assert_eq!(
            refusal.benched,
            "evaluator\u{2260}creator unsatisfiable for unit(s) [2]: team run \u{2014} no seat \
             distinct from the creator and no usable second instance (add a signed-in claude#2 \
             to the roster); a team run never grades on its creator seat"
        );
    }

    /// D1 (c): `claude#2` listed but not signed in (health not usable) — refused too. The bench
    /// names it.
    #[test]
    fn d1_c_team_run_with_an_unusable_second_instance_is_refused() {
        let err = distribute_units_against_benched(
            &team_build_and_review(),
            &[seat("claude"), unusable(seat("claude#2"))],
            "r1",
            None,
            &[],
        )
        .expect_err("an instance that is not signed in cannot grade");
        let refusal = err
            .downcast_ref::<crate::NoEligibleSeat>()
            .unwrap_or_else(|| panic!("must be NoEligibleSeat: {err:?}"));
        assert_eq!(refusal.benched_seats.len(), 1);
        assert_eq!(refusal.benched_seats[0].cli, "claude#2");
        assert!(
            refusal
                .benched
                .starts_with("evaluator\u{2260}creator unsatisfiable for unit(s) [2]"),
            "{}",
            refusal.benched
        );
    }

    /// The all-builder shape: roster `[codex, claude(, claude#2)]`, a Claude-only build on claude,
    /// an unconstrained build on codex (first seat), and an unconstrained review that routes to
    /// codex — every roster seat but `claude#2` built.
    fn all_builder_units(team: bool) -> Vec<WorkUnit> {
        let mut b1 = skilled(1, "wicked-garden-repo-learn");
        b1.stage = StageKind::Build;
        let mut b2 = WorkUnit::pending("u2", "r1", 2, "Build the other thing");
        b2.stage = StageKind::Build;
        let mut review = WorkUnit::pending("u3", "r1", 3, "Review the things");
        review.stage = StageKind::Review;
        let mut units = vec![b1, b2, review];
        for u in &mut units {
            u.team_run = team;
        }
        units
    }

    /// D1 (d): an all-builder roster behaves like (a) with a second instance, like (b) without;
    /// and (e) the NON-team run of the same shape keeps today's `creator_seat`.
    #[test]
    fn d1_d_an_all_builder_roster_behaves_like_a_or_b() {
        let (_home, _env, _) = hermetic_home("d1-all-builder-home");
        let snapshot = published("d1-all-builder");
        let without = [seat_running("codex", "codex"), claude_seat("claude")];

        // (e) twin: non-team, today's behaviour — the review stays on a builder seat, disclosed.
        let dists =
            distribute_units_against(&all_builder_units(false), &without, "r1", Some(&snapshot))
                .expect("non-team routes");
        assert_eq!(dists[0].assigned_cli, "claude");
        assert_eq!(dists[1].assigned_cli, "codex");
        assert_eq!(dists[2].assigned_cli, "codex");
        assert_eq!(
            dists[2].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_CREATOR_SEAT)
        );

        // (b) shape: team, no second instance ⇒ refused, naming the unit.
        let err =
            distribute_units_against(&all_builder_units(true), &without, "r1", Some(&snapshot))
                .expect_err("team run on an all-builder roster is refused");
        let refusal = err
            .downcast_ref::<crate::NoEligibleSeat>()
            .unwrap_or_else(|| panic!("must be NoEligibleSeat: {err:?}"));
        assert_eq!(
            refusal.benched,
            "evaluator\u{2260}creator unsatisfiable for unit(s) [3]: team run \u{2014} no seat \
             distinct from the creator and no usable second instance (add a signed-in codex#2 \
             to the roster); a team run never grades on its creator seat"
        );

        // (a) shape: team, a usable claude#2 ⇒ the review moves there, `same_cli_instance`.
        let with = [
            seat_running("codex", "codex"),
            claude_seat("claude"),
            claude_seat("claude#2"),
        ];
        let dists =
            distribute_units_against(&all_builder_units(true), &with, "r1", Some(&snapshot))
                .expect("a usable instance satisfies it");
        assert_eq!(dists[2].assigned_cli, "claude#2");
        assert_eq!(
            dists[2].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE)
        );
    }

    /// D1 (e): the NON-team launch of the (a)/(b) shape is byte-for-byte today's routing.
    #[test]
    fn d1_e_a_non_team_run_keeps_the_creator_seat_fallback() {
        let dists = distribute_units_against_benched(
            &build_and_review(),
            &[seat("claude")],
            "r1",
            None,
            &[],
        )
        .expect("a non-team run is not refused");
        assert_eq!(dists[1].assigned_cli, "claude");
        assert_eq!(
            dists[1].routing,
            RoutingInfo::Teamed {
                winner: "claude".into()
            }
        );
        assert_eq!(
            dists[1].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_CREATOR_SEAT)
        );
        // …and with a second instance, the non-team run takes it exactly as today.
        let dists = distribute_units_against_benched(
            &build_and_review(),
            &[seat("claude"), seat("claude#2")],
            "r1",
            None,
            &[],
        )
        .expect("routes");
        assert_eq!(dists[1].assigned_cli, "claude#2");
        assert_eq!(
            dists[1].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE)
        );
    }

    /// D1 (f): a bench-caused shortfall on a team run is refused exactly as today (the bench
    /// message, the bench as data).
    #[test]
    fn d1_f_a_bench_caused_team_shortfall_is_refused_as_today() {
        let roster = [seat("claude"), unusable(seat("codex"))];
        let team =
            distribute_units_against_benched(&team_build_and_review(), &roster, "r1", None, &[])
                .expect_err("refused");
        let legacy =
            distribute_units_against_benched(&build_and_review(), &roster, "r1", None, &[])
                .expect_err("refused");
        let (t, l) = (
            team.downcast_ref::<crate::NoEligibleSeat>().expect("typed"),
            legacy
                .downcast_ref::<crate::NoEligibleSeat>()
                .expect("typed"),
        );
        assert_eq!(t.benched, l.benched);
        assert_eq!(t.benched_seats, l.benched_seats);
        assert_eq!(
            t.benched,
            "evaluator\u{2260}creator unsatisfiable for unit(s) [2]: distinct seat(s) benched; \
             1 of 2 seats benched: codex (not signed in (launcher health probe) \u{2014} launcher)"
        );
    }

    /// §8.1 order for a team run: a distinct CLI before a second instance of the creator's CLI.
    /// A non-team run keeps today's roster-order pick (the instance, listed first).
    #[test]
    fn d1_a_team_run_prefers_a_distinct_cli_over_a_second_instance() {
        let roster = [seat("claude"), seat("claude#2"), seat("codex")];
        let team =
            distribute_units_against_benched(&team_build_and_review(), &roster, "r1", None, &[])
                .expect("routes");
        assert_eq!(team[1].assigned_cli, "codex");
        assert_eq!(team[1].distinctness_fallback, None);
        let legacy =
            distribute_units_against_benched(&build_and_review(), &roster, "r1", None, &[])
                .expect("routes");
        assert_eq!(legacy[1].assigned_cli, "claude#2");
        assert_eq!(
            legacy[1].distinctness_fallback.as_deref(),
            Some(DISTINCTNESS_FALLBACK_SAME_CLI_INSTANCE)
        );
    }
    /// D1 (codex on #618): a TOOL build unit is no creator seat. Its `assigned_cli` is its own
    /// program token (`tool_distribution`), which here equals the only seat key (`claude`); a
    /// team run must not read that as "the review grades its creator" and refuse the plan.
    #[test]
    fn d1_a_tool_build_whose_program_is_a_seat_key_is_no_creator_seat() {
        for team in [true, false] {
            let mut build = WorkUnit::pending("u1", "r1", 1, "Build the thing");
            build.stage = StageKind::Build;
            build.tool_cmd = Some(vec!["claude".into(), "--version".into()]);
            let mut review = WorkUnit::pending("u2", "r1", 2, "Review the thing");
            review.stage = StageKind::Review;
            let mut units = [build, review];
            for u in &mut units {
                u.team_run = team;
            }
            let dists =
                distribute_units_against_benched(&units, &[seat("claude")], "r1", None, &[])
                    .unwrap_or_else(|e| {
                        panic!("team={team}: no seated creator, nothing to refuse: {e}")
                    });
            assert_eq!(dists[0].routing, RoutingInfo::Tool);
            assert_eq!(dists[0].assigned_cli, "claude");
            assert_eq!(dists[1].assigned_cli, "claude");
            assert_eq!(
                dists[1].routing,
                RoutingInfo::Teamed {
                    winner: "claude".into()
                }
            );
            assert_eq!(dists[1].distinctness_fallback, None, "team={team}");
        }
    }
    /// D1 (codex round 5 on #618): roster `[claude, claude#2]` with `claude#2` configured but NOT
    /// usable. The remedy must name `claude#2` — signing it in satisfies the run — and never send
    /// the operator to add a `claude#3`. An unusable seat is benched (`launcher_benched`), so this
    /// refusal is the BENCH arm's: it names `claude#2` and its reason. The team arm (and its
    /// `next_instance_key` remedy) is reached only on a bench-free roster, where every configured
    /// instance is usable.
    #[test]
    fn d1_an_unusable_configured_instance_is_named_not_a_fresh_one() {
        let err = distribute_units_against_benched(
            &team_build_and_review(),
            &[seat("claude"), unusable(seat("claude#2"))],
            "r1",
            None,
            &[],
        )
        .expect_err("refused");
        let refusal = err
            .downcast_ref::<crate::NoEligibleSeat>()
            .unwrap_or_else(|| panic!("must be NoEligibleSeat: {err:?}"));
        assert_eq!(
            refusal.benched,
            "evaluator\u{2260}creator unsatisfiable for unit(s) [2]: distinct seat(s) benched; \
             1 of 2 seats benched: claude#2 (not signed in (launcher health probe) \u{2014} launcher)"
        );
        assert!(!refusal.benched.contains("claude#3"), "{}", refusal.benched);
        assert_eq!(
            err.to_string(),
            "no eligible seat for r1: evaluator\u{2260}creator unsatisfiable for unit(s) [2]: \
             distinct seat(s) benched; 1 of 2 seats benched: claude#2 (not signed in (launcher \
             health probe) \u{2014} launcher) \u{2014} sign a seat in, or add one, before launching"
        );
    }
}
