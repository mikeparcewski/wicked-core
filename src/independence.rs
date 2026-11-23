//! (DES-STANDIN-001 §3.9; core#1721) Independence before a launch: which independent roles a job
//! needs and how many real seats can fill each on the shape the launch will take, so a launcher
//! asks the person ONCE, before anything runs, when the roster is short. Diagnostic: it decides
//! nothing at run time; every role keeps its own admission at its gate.
//!
//! It also keeps the judge half it grew from (crew#972, below), which the preflight's `judges`
//! carries as the diagnostic it always was.
//!
//! (crew#972) Judge coverage: whether a launch's seats can cover an AUTHOR plus an independent JUDGE of its work,
//! asked before the launch so the launcher can say so up front instead of the run holding at a
//! judge gate mid-run.
//!
//! Not a second rule: the answer walks the runtime's own pool with the runtime's own helpers. For
//! each seat that can author governed work, its judges are the seats the DEFAULT judge rotation of a
//! gate on that seat's work walks (`cli_runner`'s `eligible_run`, then `agent_validate_in`'s order):
//! the registry records of the run's seats ([`crate::cli_runner::judge_pool`], registry order),
//! minus a record the registry marks unusable, minus the bench the run starts with
//! ([`crate::distribute::launcher_benched`] + [`crate::distribute::launch_benched`], assembled as
//! `actor.rs`'s launch check does; the ungoverned bench, as a judge is an ungoverned call), minus
//! seats sharing the author's binary ([`crate::cli_runner::without_author_binaries`]), walked by
//! [`crate::validator::distinct_judge_keys`]; the seats KNOWN by observation to call a provider the
//! author was seen calling go last ([`crate::observed_provider::shared_with`], the rule
//! `known_to_share` sorts the rotation by). Such a seat is still a judge, flagged: its approval
//! stands only if its own launch at the gate is not seen calling that provider again.
//!
//! `covered`: some author has a judge not flagged. A roster none of whose keys names a registry
//! seat (wholly ad hoc) is judged, as at the gate, from the registry's default pool, so its judges
//! may be seats outside the launch. What this does not predict, named so a launcher never says more: the fallback ladder's judge-only seats outside the run (operator ruling
//! 2026-10-14), a pinned gate's exclusion of the validator's author and its re-admission on an
//! observed launch (core#1548), team monitors (core#1165), and which seats a plan makes authors.

use serde::{Deserialize, Serialize};

use crate::domain::BenchedSeat;
use crate::AgenticCli;

/// A seat that judges one author's work, in rotation order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Judge {
    pub seat: String,
    /// Its latest observed launch called this provider, which the author's did too: tried last,
    /// and its approval stands only if the gate's launch is not seen calling it again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seen_sharing: Option<String>,
}

/// Why a seat of the run does not judge one author's work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NotJudge {
    pub seat: String,
    /// `same_identity` (it runs the author's CLI), `benched` (the run starts with it benched for
    /// every call) or `not_eligible` (no registry record, no invocation, not eligible for work, or
    /// marked unusable).
    pub reason: String,
    /// The bench's words, for `benched`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One seat that can author governed work, and who could judge it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AuthorCoverage {
    pub seat: String,
    /// The judges in the order the runtime rotation tries them.
    pub judges: Vec<Judge>,
    /// (core#1594) Exactly one judge has not been seen sharing the author's provider.
    pub sole_judge: Option<String>,
    /// Every other seat of the run, with why it does not judge this author's work.
    pub not_judges: Vec<NotJudge>,
}

/// [`judge_coverage`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeCoverage {
    /// Some work seat's work has a judge that is not [seen sharing](Judge::seen_sharing) its provider.
    pub covered: bool,
    /// Every work seat runs one CLI identity (the one-CLI quickstart).
    pub single_identity: bool,
    /// The seats that can author governed work, in launch order.
    pub authors: Vec<AuthorCoverage>,
    /// The seats the run would start with benched, with the bench's words.
    pub benched: Vec<NotJudge>,
    /// (core#1594) Provider-sharing observations the launcher can say before launching.
    pub notes: Vec<String>,
}

/// See the module doc. `clis`: the launch roster (as `launchRun`'s `clisJson`); `bound`: the run is
/// bound to a repository (the launch's worktree bench). The judges' records are this process's
/// registry, as at the gate.
pub fn judge_coverage(clis: &[AgenticCli], bound: bool) -> JudgeCoverage {
    judge_coverage_in(
        clis,
        &crate::registry_roster(),
        bound,
        true,
        &crate::observed_provider::last_seen,
    )
}

/// The bench the run would start with: the launcher's word, then the engine's host-floor and
/// worktree benches, assembled as the launch does (`actor.rs`, the `NoEligibleSeat` check; a
/// consumer's question is about a file-backed daemon, whose governed units are armed).
pub(crate) fn launch_bench(clis: &[AgenticCli], bound: bool, contained: bool) -> Vec<BenchedSeat> {
    let mut bench: Vec<BenchedSeat> = crate::distribute::launcher_benched(clis);
    for seat in crate::distribute::launch_benched(clis, bound, contained) {
        crate::domain::bench_seat(&mut bench, seat);
    }
    bench
}

/// [`judge_coverage`] over an explicit `registry` (the gate's `registry_roster()`).
pub(crate) fn judge_coverage_in(
    clis: &[AgenticCli],
    registry: &[AgenticCli],
    bound: bool,
    contained: bool,
    seen: &dyn Fn(&str) -> Option<Vec<String>>,
) -> JudgeCoverage {
    let bench = launch_bench(clis, bound, contained);
    let judge_bench = crate::domain::ungoverned_bench(&bench);
    let run_roster: Vec<String> = clis.iter().map(|c| c.key.clone()).collect();
    // The default judge's pool, as the gate builds it (`eligible_run`).
    let pool = crate::cli_runner::judge_pool(registry, &run_roster);
    let run_has_registry_seat = registry
        .iter()
        .any(|c| crate::cli_runner::in_run_roster(&run_roster, &c.key));
    let eligible_run: Vec<AgenticCli> = pool
        .iter()
        .filter(|c| c.health.as_ref().is_none_or(|h| h.usable))
        .filter(|c| !judge_bench.iter().any(|b| b.cli == c.key))
        .filter(|c| !run_has_registry_seat || crate::cli_runner::in_run_roster(&run_roster, &c.key))
        .map(|c| (*c).clone())
        .collect();
    let benched_any = |k: &str| bench.iter().any(|b| b.cli == k);
    let can_work =
        |c: &AgenticCli| c.seat_eligible_for_work && !c.headless_invocation.trim().is_empty();

    let authors: Vec<AuthorCoverage> = clis
        .iter()
        .filter(|c| can_work(c) && !benched_any(&c.key))
        .map(|author| {
            // As the gate excludes a dispatched unit's author: its seat, then (from `authored_by`)
            // instance and CLI key, so an instance's rotation anchors on its CLI's record.
            let cli_key = crate::team::runner::seat_key(&author.key);
            let ex: Vec<&str> = if cli_key == author.key {
                vec![author.key.as_str()]
            } else {
                vec![author.key.as_str(), cli_key]
            };
            let ids = crate::cli_runner::author_identities(&ex, registry);
            let roster = crate::cli_runner::without_author_binaries(&eligible_run, &ex, &ids);
            let avoid = seen(&author.key).unwrap_or_default();
            let mut judges: Vec<Judge> = crate::validator::distinct_judge_keys(&ex, &roster)
                .into_iter()
                .map(|seat| Judge {
                    // `observed_provider::shared_with` over `seen`.
                    seen_sharing: seen(&seat)
                        .and_then(|ps| ps.into_iter().find(|p| avoid.contains(p))),
                    seat,
                })
                .collect();
            // The rotation's own order: a stable sort, seats known to share last.
            judges.sort_by_key(|j| j.seen_sharing.is_some());
            // A judge is a registry record; a configured instance seat is judged through its CLI's.
            let on_record =
                |c: &AgenticCli, k: &str| k == c.key || k == crate::team::runner::seat_key(&c.key);
            let not_judges = clis
                .iter()
                .filter(|c| c.key != author.key && !judges.iter().any(|j| on_record(c, &j.seat)))
                .map(|c| {
                    let (reason, detail) =
                        if let Some(b) = judge_bench.iter().find(|b| b.cli == c.key) {
                            ("benched", Some(b.reason.clone()))
                        } else if !eligible_run
                            .iter()
                            .any(|e| on_record(c, &e.key) && can_work(e))
                        {
                            ("not_eligible", None)
                        } else {
                            ("same_identity", None)
                        };
                    NotJudge {
                        seat: c.key.clone(),
                        reason: reason.into(),
                        detail,
                    }
                })
                .collect();
            AuthorCoverage {
                seat: author.key.clone(),
                sole_judge: {
                    let mut independent = judges.iter().filter(|j| j.seen_sharing.is_none());
                    independent
                        .next()
                        .filter(|_| independent.next().is_none())
                        .map(|j| j.seat.clone())
                },
                judges,
                not_judges,
            }
        })
        .collect();

    let identities: std::collections::HashSet<String> = clis
        .iter()
        .filter(|c| authors.iter().any(|a| a.seat == c.key))
        .map(crate::validator::seat_identity)
        .collect();
    let notes = authors
        .iter()
        .filter_map(|a| {
            let sharing = a
                .judges
                .iter()
                .filter_map(|j| {
                    j.seen_sharing.as_ref().map(|p| {
                        format!(
                            "{} was seen calling {p}, the provider {} calls",
                            j.seat, a.seat,
                        )
                    })
                })
                .collect::<Vec<_>>();
            if sharing.is_empty() {
                return None;
            }
            let consequence = match &a.sole_judge {
                Some(seat) => format!("only {seat} can judge {}'s work", a.seat),
                None if a.judges.iter().all(|j| j.seen_sharing.is_some()) => {
                    format!("no seat can judge {}'s work independently", a.seat)
                }
                None => format!(
                    "{} can judge {}'s work independently",
                    a.judges
                        .iter()
                        .filter(|j| j.seen_sharing.is_none())
                        .map(|j| j.seat.as_str())
                        .collect::<Vec<_>>()
                        .join(" and "),
                    a.seat,
                ),
            };
            Some(format!("{}, so {consequence}.", sharing.join("; ")))
        })
        .collect();
    JudgeCoverage {
        covered: authors
            .iter()
            .any(|a| a.judges.iter().any(|j| j.seen_sharing.is_none())),
        single_identity: identities.len() == 1,
        notes,
        benched: bench
            .iter()
            .map(|b| NotJudge {
                seat: b.cli.clone(),
                reason: "benched".into(),
                detail: Some(b.reason.clone()),
            })
            .collect(),
        authors,
    }
}

// ── The preflight (DES-STANDIN-001 §3.9, rev 3) ─────────────────────────────────────────────────

/// A launch option the launch reads as optional: absent and `null` are its default, as at the
/// launch boundary (`LaunchOptions`' `Option` fields).
fn null_default<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Option::<T>::deserialize(d).map(Option::unwrap_or_default)
}

/// The launch options that decide a job's contract and shape, as `launchRun` reads them
/// (`IndependencePreflight`'s `shapeJson`). Every field is optional on the wire; `null` is absent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PreflightShape {
    /// A preset name or a registered workflow id, as the launch's `workflow`.
    pub workflow: Option<String>,
    pub project_id: Option<String>,
    /// A user-composed plan, as `launchRun`'s `planJson`: the launch is a team run.
    pub plan_json: Option<String>,
    /// Accepted for parity with the launch options; it changes no role (a power launch drops
    /// pre-build phases, never a review).
    #[serde(deserialize_with = "null_default")]
    pub power: bool,
    /// The launch's reduced-assurance opt-in (it lowers the contract's floors).
    #[serde(deserialize_with = "null_default")]
    pub reduced_assurance: bool,
    /// The PA seat the launcher chose (refused as the launch refuses it). Absent: every work seat
    /// is tried as the author and each role reports its worst case.
    pub primary: Option<String>,
    /// Seats that authored the work under review (review-change) or ran the work in the window
    /// (insights): never real seats for an independent role of this job.
    #[serde(deserialize_with = "null_default")]
    pub parties: Vec<String>,
    /// The launch's judge exclusions (`excludeSeats`).
    #[serde(deserialize_with = "null_default")]
    pub exclude_seats: Vec<String>,
    pub skip_qe_acceptance_reason: Option<String>,
    #[serde(deserialize_with = "null_default")]
    pub force_qe_acceptance: bool,
    /// The launch is bound to a repository (its worktree bench, and its steering rules).
    #[serde(deserialize_with = "null_default")]
    pub repo_bound: bool,
    /// The council size (`WICKED_COUNCIL_SIZE` when absent), bounded as that setting is.
    pub council_size: Option<usize>,
}

/// The contract and host facts the launch would run under, resolved by the engine
/// ([`crate::Core::independence_preflight`]); the launcher never predicts them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PreflightContract {
    pub evaluator: bool,
    pub judge: bool,
    pub qe: bool,
    /// A plan or a preset launch: a team run, with its councils and monitors.
    pub team: bool,
    /// A repo-bound launch with at least one steering rule in force for its project.
    pub rules: bool,
    /// The engine's governed units are armed (`in_process_governance`): the launch's benches.
    pub contained: bool,
}

/// Resolve the contract `shape` would launch under, through the launch's own code (`actor.rs`, the
/// launch's sync fast path): a plan or a preset is a team run whose instruments are the preset's
/// declaration (`preset::required_instruments`), else the registered def's `required_instruments`;
/// then the QE word, refused as the launch refuses it. A team run that is not reduced never grades
/// its own work, whatever its instruments (`distribute.rs`, `allow_same_seat`). Reads the store:
/// the actor calls it.
pub(crate) fn preflight_contract(
    store: &dyn wicked_apps_core::GraphRead,
    registry: &crate::workflow::WorkflowRegistry,
    shape: &PreflightShape,
) -> anyhow::Result<PreflightContract> {
    let plan = shape
        .plan_json
        .as_deref()
        .map(serde_json::from_str::<crate::plan::PlanSteps>)
        .transpose()
        .map_err(|e| anyhow::anyhow!("planJson is not a valid plan: {e}"))?;
    let qe = crate::assurance::QeOverride::from_wire(
        shape.skip_qe_acceptance_reason.clone(),
        shape.force_qe_acceptance,
    )
    .map_err(anyhow::Error::msg)?;
    let project = shape.project_id.as_deref();
    let launch = crate::plan_gate::launch_plan(
        store,
        plan.as_ref(),
        shape.workflow.as_deref(),
        project,
        &[],
    )?;
    let team = launch.is_some();
    let instruments = match &launch {
        Some((_, Some(preset))) => crate::preset::required_instruments(store, project, preset)?,
        Some((_, None)) => None,
        None => crate::pipeline::resolve_workflow_def(
            store,
            project,
            shape.workflow.as_deref(),
            Some(registry),
        )?
        .and_then(|d| d.required_instruments),
    };
    let assurance =
        crate::assurance::RunAssurance::new(instruments.as_deref(), shape.reduced_assurance)
            .with_qe_override(&qe)
            .map_err(anyhow::Error::msg)?;
    let rules = shape.repo_bound && {
        let query = wicked_governance::RuleQuery {
            projects: project.map(|p| vec![p.to_string()]).unwrap_or_default(),
            ..Default::default()
        };
        !wicked_governance::consider_rules(store, &query)?
            .in_force
            .is_empty()
    };
    Ok(PreflightContract {
        evaluator: assurance.enforces(crate::assurance::DISTINCT_EVALUATOR)
            || (team && !assurance.reduced()),
        judge: assurance.enforces(crate::assurance::JUDGE),
        qe: assurance.enforces(crate::assurance::QE_ACCEPTANCE),
        team,
        rules,
        contained: false,
    })
}

/// One independent role the job may need.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RolePreflight {
    /// `evaluator` | `judge` | `rule_judge` | `qe_reviewer` | `council` | `rule_council` |
    /// `monitor` | `party_excluded`.
    pub role: String,
    pub needed: usize,
    /// The real seats that can fill it on the expected shape, in admission order.
    pub real: Vec<String>,
    pub short_by: usize,
    /// Some signed-in seat could perform the role as a stand-in (its base).
    pub can_stand_in: bool,
    pub stand_in_from: Option<StandInFrom>,
    /// Needed only if an event of the run calls for it (a dispute, a band, a rule disputed).
    pub conditional: bool,
    /// Why the real seats are short, in words; `None` when not short.
    pub why: Option<String>,
}

/// The base a stand-in for a role would run on, and the models it would use (`null`: the CLI's
/// default model; model choice lands with the registry's `review_models`, DES §3.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StandInFrom {
    pub cli: String,
    pub models: Vec<Option<String>>,
}

/// Why a job is short, as data (DES §3.7): never parsed from the words. The stand-in reason
/// ([`crate::standin::Reason`]); a preflight produces only its first three kinds.
pub type ShortReason = crate::standin::Reason;

/// (core#1721) Provider diversity among the signed-in work seats.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diversity {
    /// Distinct providers the seats reach: those they were seen calling, and one per CLI for a
    /// seat never observed.
    pub providers: usize,
    /// Independent providers the job's reviews need (the author's and a reviewer's).
    pub needed: usize,
    /// …plus one to survive a failover.
    pub needed_with_failover: usize,
    /// Seats seen calling one provider.
    pub sharing: Vec<ProviderSharing>,
    /// A registry helper not signed in that runs none of the signed-in CLIs and was not seen
    /// calling their providers.
    pub outside_helper: Option<String>,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProviderSharing {
    pub seats: Vec<String>,
    pub provider: String,
}

/// [`independence_preflight_in`]'s answer (`Core.independencePreflight`'s JSON).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndependencePreflight {
    /// Some role (conditional ones included) has fewer real seats than it needs.
    pub short: bool,
    /// The PA seat the launcher chose; `None` when the launch draws it (each role's worst case).
    pub primary: Option<String>,
    pub roles: Vec<RolePreflight>,
    /// The first short role's reason in words; `None` when not short.
    pub reason: Option<String>,
    pub reason_data: Option<ShortReason>,
    /// The work seats the run would start with (signed in, not benched).
    pub signed_in: Vec<String>,
    pub benched: Vec<NotJudge>,
    /// Today's judge coverage, per author (diagnostic only).
    pub judges: Vec<AuthorCoverage>,
    pub diversity: Diversity,
}

/// What the preflight reads from the host besides the roster: the registry (the process's
/// `registry_roster()`), the skills routing snapshot (QE admission) and each seat's latest observed
/// providers. Explicit so a test reads only its own.
pub(crate) struct HostFacts<'a> {
    pub registry: &'a [AgenticCli],
    pub skills: Option<&'a crate::skills_snapshot::SkillsSnapshot>,
    pub seen: &'a dyn Fn(&str) -> Option<Vec<String>>,
}

/// Seats a list names, as the gate excludes a work author: by key or cli key (an instance or its
/// cli), and by the binary the named record runs (`validator::excluded_identity` over the roster
/// and the registry, so an alias of a named CLI is named too).
struct Named {
    keys: Vec<String>,
    ids: Vec<String>,
}

impl Named {
    fn new(keys: Vec<String>, records: &[AgenticCli]) -> Self {
        let ids = keys
            .iter()
            .map(|k| crate::validator::excluded_identity(k, records))
            .collect();
        Named { keys, ids }
    }

    fn key(&self, seat: &str) -> bool {
        use wicked_apps_core::spawn::seat_id::family;
        self.keys
            .iter()
            .any(|p| p == seat || family(p) == family(seat))
    }

    fn names(&self, c: &AgenticCli) -> bool {
        self.key(&c.key) || self.ids.contains(&crate::validator::seat_identity(c))
    }

    /// A seat known only by its key (a judge): its record's binary when there is one.
    fn names_key(&self, seat: &str, records: &[AgenticCli]) -> bool {
        self.key(seat)
            || self
                .ids
                .contains(&crate::validator::excluded_identity(seat, records))
    }
}

/// The preflight over `clis` (the launch roster) and the contract the engine resolved.
pub(crate) fn independence_preflight_in(
    clis: &[AgenticCli],
    host: &HostFacts<'_>,
    shape: &PreflightShape,
    contract: PreflightContract,
) -> anyhow::Result<IndependencePreflight> {
    let identity = crate::validator::seat_identity;
    // The PA the launcher chose, refused as the launch refuses it (`team_gate::pick_primary`).
    let chosen = match shape.primary.as_deref() {
        None => None,
        Some(key) => match clis.iter().find(|c| c.key == key) {
            Some(c) if c.seat_eligible_for_work => Some(c),
            Some(_) => anyhow::bail!(
                "primary seat `{key}` is a ballot-only seat (seat_eligible_for_work = false) and \
                 cannot run the path's work"
            ),
            None => anyhow::bail!(
                "primary seat `{key}` is not on the roster [{}]",
                clis.iter()
                    .map(|c| c.key.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        },
    };
    let records: Vec<AgenticCli> = clis.iter().chain(host.registry).cloned().collect();
    let bench = launch_bench(clis, shape.repo_bound, contract.contained);
    // A council call is not governed work: only the bench that holds for every call stops a voter
    // (`actor::council_roster`), as for a judge.
    let ungoverned = crate::domain::ungoverned_bench(&bench);
    let health = |c: &AgenticCli| c.health.as_ref().is_none_or(|h| h.usable);
    let can_work =
        |c: &AgenticCli| c.seat_eligible_for_work && !c.headless_invocation.trim().is_empty();
    let workers: Vec<&AgenticCli> = clis
        .iter()
        .filter(|c| health(c) && can_work(c) && !bench.iter().any(|b| b.cli == c.key))
        .collect();
    let voters: Vec<&AgenticCli> = clis
        .iter()
        .filter(|c| {
            health(c) && c.enabled_for_council && !ungoverned.iter().any(|b| b.cli == c.key)
        })
        .collect();
    let coverage = judge_coverage_in(
        clis,
        host.registry,
        shape.repo_bound,
        contract.contained,
        host.seen,
    );
    let council_size = match shape.council_size {
        Some(n) => wicked_council::pick::council_size_from(Some(&n.to_string())),
        None => wicked_council::pick::council_size(),
    };
    let parties = Named::new(
        crate::domain::normalize_exclude_seats(&shape.parties),
        &records,
    );
    let excluded = Named::new(
        crate::domain::normalize_exclude_seats(&shape.exclude_seats),
        &records,
    );
    let qe_admits = |c: &AgenticCli| {
        crate::distribute::qe_reviewer_admitted_against(
            host.skills,
            &c.key,
            Some(c.headless_invocation.clone()),
        )
    };
    let work_base = workers.first().map(|c| c.key.clone());
    let qe_base = workers.iter().find(|c| qe_admits(c)).map(|c| c.key.clone());
    let vote_base = voters.first().map(|c| c.key.clone());

    // The roles for one author: `None` for the author means no seat can author at all.
    let roles_for = |author: Option<&AgenticCli>| -> Vec<RolePreflight> {
        let author_id = author.map(identity);
        let author_seen = author.and_then(|a| (host.seen)(&a.key)).unwrap_or_default();
        let shares_author = |c: &AgenticCli| {
            (host.seen)(&c.key).is_some_and(|ps| ps.iter().any(|p| author_seen.contains(p)))
        };
        let evaluators: Vec<&AgenticCli> = workers
            .iter()
            .copied()
            .filter(|c| author_id.as_ref().is_none_or(|a| identity(c) != *a) && !parties.names(c))
            .collect();
        let keys = |cs: &[&AgenticCli]| cs.iter().map(|c| c.key.clone()).collect::<Vec<_>>();
        let judges: Vec<String> = author
            .and_then(|a| coverage.authors.iter().find(|x| x.seat == a.key))
            .map(|a| {
                a.judges
                    .iter()
                    .filter(|j| {
                        j.seen_sharing.is_none()
                            && !parties.names_key(&j.seat, &records)
                            && !excluded.names_key(&j.seat, &records)
                    })
                    .map(|j| j.seat.clone())
                    .collect()
            })
            .unwrap_or_default();
        // QE admission (`qe_acceptance/review.rs`): not excluded, admitted by the QE skill's
        // routing, not seen calling the author's provider.
        let qe_reviewers: Vec<&AgenticCli> = evaluators
            .iter()
            .copied()
            .filter(|c| !excluded.names(c) && qe_admits(c) && !shares_author(c))
            .collect();
        // The expected shape (DES §2.2, §2.4): the first reviewer also judges when it can, so a
        // council's parties are the launch's parties, the author and that reviewer (plus the
        // judge when the reviewer cannot judge).
        let reviewer = if contract.evaluator {
            evaluators.first().map(|c| c.key.clone())
        } else {
            None
        };
        let judge = match &reviewer {
            _ if !contract.judge => None,
            Some(r) if judges.contains(r) => Some(r.clone()),
            _ => judges.first().cloned(),
        };
        let mut seated: Vec<String> = author.map(|a| a.key.clone()).into_iter().collect();
        seated.extend(reviewer);
        seated.extend(judge);
        let council_parties = Named::new(seated, &records);
        let council_voters = |extra: &Named, shares: &dyn Fn(&AgenticCli) -> bool| {
            let mut ids: Vec<String> = Vec::new();
            voters
                .iter()
                .filter(|c| {
                    !parties.names(c) && !council_parties.names(c) && !extra.names(c) && !shares(c)
                })
                .filter(|c| {
                    // One voter per identity: the council resolves a seat by its CLI's record.
                    let id = identity(c);
                    let fresh = !ids.contains(&id);
                    if fresh {
                        ids.push(id);
                    }
                    fresh
                })
                .map(|c| c.key.clone())
                .collect::<Vec<_>>()
        };
        let none = Named::new(Vec::new(), &records);
        let council = council_voters(&none, &|_: &AgenticCli| false);
        let non_party_workers: Vec<String> = workers
            .iter()
            .filter(|c| !parties.names(c))
            .map(|c| c.key.clone())
            .collect();
        let mut roles = Vec::new();
        let mut push = |role: &str,
                        needed: usize,
                        real: Vec<String>,
                        base: &Option<String>,
                        conditional: bool| {
            let short_by = needed.saturating_sub(real.len());
            let name = role.replace('_', " ");
            roles.push(RolePreflight {
                role: role.to_string(),
                needed,
                why: (short_by > 0).then(|| match real.len() {
                    0 => {
                        format!("no signed-in helper that did not build the work can be the {name}")
                    }
                    n => format!("{n} of {needed} can be the {name}"),
                }),
                real,
                short_by,
                can_stand_in: base.is_some(),
                stand_in_from: base.as_ref().map(|cli| StandInFrom {
                    cli: cli.clone(),
                    models: vec![None],
                }),
                conditional,
            });
        };
        if contract.evaluator {
            push("evaluator", 1, keys(&evaluators), &work_base, false);
        }
        if contract.judge {
            push("judge", 1, judges, &work_base, false);
        }
        if contract.qe {
            push("qe_reviewer", 1, keys(&qe_reviewers), &qe_base, false);
        }
        if shape.repo_bound && contract.rules {
            // The rule judge (`cli_runner.rs`, `rule_judge_seats`): not excluded at launch, not the
            // author's binary. The rule council (`cli_runner.rs`, the rule council's seats): its
            // parties are the expected shape's plus the rule judge, launch exclusions apply, and a
            // seat seen calling the author's provider does not vote.
            let rule_judges: Vec<&AgenticCli> = evaluators
                .iter()
                .copied()
                .filter(|c| !excluded.names(c))
                .collect();
            let mut rule_ex = excluded.keys.clone();
            rule_ex.extend(rule_judges.first().map(|c| c.key.clone()));
            let rule_ex = Named::new(rule_ex, &records);
            push("rule_judge", 1, keys(&rule_judges), &work_base, true);
            push(
                "rule_council",
                crate::decision::RULE_COUNCIL_SIZE,
                council_voters(&rule_ex, &shares_author),
                &vote_base,
                true,
            );
        }
        if contract.team {
            push("council", council_size, council, &vote_base, true);
            let target = usize::from(crate::team::supervisor::monitor_target(None, 0));
            push("monitor", target, keys(&evaluators), &work_base, true);
        }
        if !parties.keys.is_empty() {
            push("party_excluded", 1, non_party_workers, &work_base, false);
        }
        roles
    };

    // The author: the launcher's PA, else every work seat (a random draw may meet any). Each role
    // reports its worst case over those authors.
    let candidates: Vec<Option<&AgenticCli>> = match chosen {
        Some(c) => vec![Some(c)],
        None if workers.is_empty() => vec![None],
        None => workers.iter().copied().map(Some).collect(),
    };
    let shapes: Vec<Vec<RolePreflight>> = candidates.into_iter().map(roles_for).collect();
    let roles: Vec<RolePreflight> = (0..shapes.first().map_or(0, Vec::len))
        .filter_map(|i| {
            shapes
                .iter()
                .filter_map(|s| s.get(i))
                .reduce(|worst, r| {
                    if r.short_by > worst.short_by {
                        r
                    } else {
                        worst
                    }
                })
                .cloned()
        })
        .collect();

    let signed_in: Vec<String> = workers.iter().map(|c| c.key.clone()).collect();
    let display = |key: &str| {
        records
            .iter()
            .find(|c| c.key == key || c.key == wicked_apps_core::spawn::seat_id::family(key))
            .map(|c| c.display_name.clone())
            .unwrap_or_else(|| key.to_string())
    };
    let identities: std::collections::BTreeSet<String> =
        workers.iter().map(|c| identity(c)).collect();
    let short = roles.iter().any(|r| r.short_by > 0);
    let reason_data = short.then(|| {
        if identities.len() == 1 {
            ShortReason::OnlyOneHelper {
                cli: display(&signed_in[0]),
            }
        } else if !signed_in.is_empty() && workers.iter().all(|c| parties.names(c)) {
            ShortReason::AllParties {
                seats: signed_in.iter().map(|k| display(k)).collect(),
            }
        } else {
            let first = roles.iter().find(|r| r.short_by > 0).expect("short");
            ShortReason::HelpersShort {
                signed_in: signed_in.len(),
                need: first.needed,
                have: first.real.len(),
            }
        }
    });
    Ok(IndependencePreflight {
        short,
        primary: chosen.map(|c| c.key.clone()),
        reason: reason_data.as_ref().map(ShortReason::words),
        reason_data,
        signed_in,
        benched: coverage.benched.clone(),
        diversity: diversity(&workers, clis, host, &contract),
        judges: coverage.authors,
        roles,
    })
}

/// What one work seat reaches: the providers it was seen calling (`seen`), else its CLI as its
/// own provider (`reached`).
struct Reach<'a> {
    seat: &'a str,
    seen: Option<Vec<String>>,
    reached: Vec<String>,
}

/// (core#1721) How many independent providers the signed-in work seats reach, which of them were
/// seen calling one provider, and who could add one.
fn diversity(
    workers: &[&AgenticCli],
    clis: &[AgenticCli],
    host: &HostFacts<'_>,
    contract: &PreflightContract,
) -> Diversity {
    let identity = crate::validator::seat_identity;
    // What each seat reaches: the providers it was seen calling, else its CLI (never observed).
    let reach: Vec<Reach<'_>> = workers
        .iter()
        .map(|c| {
            let seen = (host.seen)(&c.key).filter(|p| !p.is_empty());
            let reached = seen
                .clone()
                .unwrap_or_else(|| vec![format!("cli:{}", identity(c))]);
            Reach {
                seat: c.key.as_str(),
                seen,
                reached,
            }
        })
        .collect();
    // Independent providers: seats picked in roster order, each sharing nothing with one picked
    // before it, so overlap never counts twice (`{X,Y}` and `{X,Y}` are one, not two).
    let mut taken: Vec<&String> = Vec::new();
    let mut independent = 0;
    for r in &reach {
        if !r.reached.iter().any(|p| taken.contains(&p)) {
            independent += 1;
            taken.extend(&r.reached);
        }
    }
    // Sharing: only what was observed, per provider, in roster order.
    let mut observed: Vec<&String> = Vec::new();
    for p in reach.iter().filter_map(|r| r.seen.as_ref()).flatten() {
        if !observed.contains(&p) {
            observed.push(p);
        }
    }
    let sharing: Vec<ProviderSharing> = observed
        .into_iter()
        .filter_map(|p| {
            let seats: Vec<String> = reach
                .iter()
                .filter(|r| r.seen.as_ref().is_some_and(|s| s.contains(p)))
                .map(|r| r.seat.to_string())
                .collect();
            (seats.len() > 1).then(|| ProviderSharing {
                seats,
                provider: p.clone(),
            })
        })
        .collect();
    let needed = if contract.evaluator || contract.judge || contract.qe {
        2
    } else {
        1
    };
    let needed_with_failover = needed + 1;
    let ids: Vec<String> = clis.iter().map(identity).collect();
    let outside_helper = host
        .registry
        .iter()
        .find(|r| {
            r.seat_eligible_for_work
                && !r.headless_invocation.trim().is_empty()
                && !ids.contains(&identity(r))
                && (host.seen)(&r.key).is_none_or(|ps| !ps.iter().any(|p| taken.contains(&p)))
        })
        .map(|r| r.display_name.clone());
    let note = (independent < needed_with_failover && !workers.is_empty()).then(|| {
        let mut note = String::new();
        for s in &sharing {
            note.push_str(&format!(
                "{} were seen calling {}, one provider. ",
                s.seats.join(" and "),
                s.provider
            ));
        }
        note.push_str(&format!(
            "This job has {independent} independent provider{}; its reviews need {needed}, plus 1 \
             to survive one failover.",
            if independent == 1 { "" } else { "s" }
        ));
        if let Some(h) = &outside_helper {
            note.push_str(&format!(" Signing in {h} could add another."));
        }
        note
    });
    Diversity {
        providers: independent,
        needed,
        needed_with_failover,
        sharing,
        outside_helper,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observed_provider::{record_receipt, take_receipts};

    fn seat(key: &str, invocation: &str) -> AgenticCli {
        use wicked_council::{Category, Confidence, InputMode};
        AgenticCli {
            key: key.into(),
            display_name: key.into(),
            binary: "unused".into(),
            headless_invocation: invocation.into(),
            category: Category::default(),
            input_mode: InputMode::default(),
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::default(),
            enabled_for_council: true,
            seat_eligible_for_work: true,
            acp: None,
            capabilities: None,
            login_invocation: None,
            logout_invocation: None,
            governance_class: None,
            credential: None,
            free_tier: None,
            health: None,
        }
    }

    /// Runs `f` with the container floor pinned (the benches read the floor; other tests change it).
    fn on_container_floor<T>(f: impl FnOnce() -> T) -> T {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var_os(crate::container_floor::SEAT_FLOOR_ENV);
        std::env::set_var(crate::container_floor::SEAT_FLOOR_ENV, "container");
        let out = f();
        match prev {
            Some(v) => std::env::set_var(crate::container_floor::SEAT_FLOOR_ENV, v),
            None => std::env::remove_var(crate::container_floor::SEAT_FLOOR_ENV),
        }
        out
    }

    #[test]
    fn a_seat_seen_on_the_authors_provider_judges_last_and_flagged() {
        let clis = [
            seat("jcu1-claude", "claude -p {PROMPT}"),
            seat("jcu1-pi", "pi -p {PROMPT}"),
            seat("jcu1-codex", "codex -p {PROMPT}"),
        ];
        record_receipt("jcu1-claude", Some(vec!["api.anthropic.com:443".into()]));
        record_receipt("jcu1-pi", Some(vec!["api.anthropic.com:443".into()]));
        record_receipt("jcu1-codex", Some(vec!["chatgpt.com:443".into()]));
        let _ = take_receipts();
        let cov = on_container_floor(|| {
            judge_coverage_in(
                &clis,
                &clis,
                false,
                true,
                &crate::observed_provider::last_seen,
            )
        });
        assert!(cov.covered);
        let claude = &cov.authors[0];
        assert_eq!(claude.judges[0].seat, "jcu1-codex");
        assert_eq!(claude.judges[1].seat, "jcu1-pi");
        assert_eq!(
            claude.judges[1].seen_sharing.as_deref(),
            Some("api.anthropic.com:443")
        );
        assert_eq!(claude.sole_judge.as_deref(), Some("jcu1-codex"));
        assert!(
            cov.notes.iter().any(|n| {
                n.contains("jcu1-pi was seen calling api.anthropic.com:443")
                    && n.contains("only jcu1-codex can judge jcu1-claude's work")
            }),
            "{:?}",
            cov.notes
        );
    }

    #[test]
    fn coverage_names_when_every_judge_was_seen_sharing() {
        let clis = [
            seat("jcu3-author", "claude {PROMPT}"),
            seat("jcu3-judge", "pi {PROMPT}"),
        ];
        for cli in &clis {
            record_receipt(&cli.key, Some(vec!["coverage-shared:443".into()]));
        }
        take_receipts();
        let cov = on_container_floor(|| {
            judge_coverage_in(
                &clis,
                &clis,
                false,
                true,
                &crate::observed_provider::last_seen,
            )
        });
        assert_eq!(cov.authors[0].sole_judge, None);
        assert!(cov
            .notes
            .iter()
            .any(|n| { n.contains("no seat can judge jcu3-author's work independently") }));
    }

    #[test]
    fn a_launcher_benched_or_ballot_only_seat_neither_authors_nor_judges() {
        let mut clis = [
            seat("jcu2-claude", "claude -p {PROMPT}"),
            seat("jcu2-codex", "codex -p {PROMPT}"),
            seat("jcu2-pi", "pi -p {PROMPT}"),
        ];
        clis[1].health = Some(wicked_council::types::SeatHealth::unusable("signed out"));
        clis[2].seat_eligible_for_work = false;
        let registry = clis.clone();
        let cov = on_container_floor(|| {
            judge_coverage_in(
                &clis,
                &registry,
                false,
                true,
                &crate::observed_provider::last_seen,
            )
        });
        assert!(!cov.covered);
        assert_eq!(cov.authors.len(), 1);
        let reasons: Vec<&str> = cov.authors[0]
            .not_judges
            .iter()
            .map(|n| n.reason.as_str())
            .collect();
        assert_eq!(reasons, ["benched", "not_eligible"]);
        assert_eq!(cov.benched[0].detail.as_deref(), Some("signed out"));
    }

    // ── The preflight (DES-STANDIN-001 §3.9; T-R2, T-R3) ─────────────────────────────────────

    /// A seat as the launch receives it from crew: the engine's own registry record
    /// (`wicked_council::registry::builtin`, crates/wicked-council/src/registry.rs:164) passed through
    /// crew's `toEngineSeat` (wicked-crew packages/crew/src/core/engine-roster.ts:129-150), which
    /// keeps the record and adds `health: {usable: true}` for a signed-in seat (:140-141). Re-keyed
    /// with `tag` because the judge rotation reads the process-wide observed providers, which other
    /// tests record under the production keys (`claude`, `codex`, `pi`, `opencode`); every test here
    /// passes the same re-keyed records as the registry, so roster and registry agree as they do
    /// in production. The identity is the record's invocation, which the re-key keeps.
    fn roster_seat(tag: &str, key: &str) -> AgenticCli {
        let mut c = wicked_council::registry::builtin()
            .into_iter()
            .find(|c| c.key == key)
            .unwrap_or_else(|| panic!("no built-in seat `{key}`"));
        c.key = format!("{tag}-{key}");
        c.health = Some(wicked_council::types::SeatHealth {
            usable: true,
            reason: None,
        });
        c
    }

    fn never_seen(_: &str) -> Option<Vec<String>> {
        None
    }

    fn host<'a>(
        registry: &'a [AgenticCli],
        seen: &'a dyn Fn(&str) -> Option<Vec<String>>,
    ) -> HostFacts<'a> {
        HostFacts {
            registry,
            skills: None,
            seen,
        }
    }

    fn role<'a>(p: &'a IndependencePreflight, name: &str) -> &'a RolePreflight {
        p.roles
            .iter()
            .find(|r| r.role == name)
            .unwrap_or_else(|| panic!("no `{name}` role in {:?}", p.roles))
    }

    fn role_names(p: &IndependencePreflight) -> Vec<&str> {
        p.roles.iter().map(|r| r.role.as_str()).collect()
    }

    /// The contract a launch of `shape` resolves to through the launch's own code, over a store
    /// seeded as the daemon seeds it at boot (`preset::seed_builtins`, `actor.rs`), with one steering
    /// rule in force when `rule` (registered by the store writer the actor's
    /// `UpsertConformanceRule` uses, `wicked_governance::register_rule`).
    fn contract_for(shape: &PreflightShape, rule: bool) -> PreflightContract {
        let mut store = wicked_apps_core::open_store(Some(":memory:")).unwrap();
        crate::preset::seed_builtins(&mut store, 10).unwrap();
        if rule {
            wicked_governance::register_rule(
                &mut store,
                &wicked_governance::ConformanceRule {
                    id: "R1".into(),
                    rule_type: wicked_governance::RuleType::Pattern,
                    statement: "every fix ships a test".into(),
                    severity: wicked_governance::ConfSeverity::Warn,
                    ..Default::default()
                },
            )
            .unwrap();
        }
        let registry = crate::workflow::WorkflowRegistry::default();
        // The actor adds the host fact: a file-backed daemon's governed units are armed
        // (`in_process_governance`), so the launch's benches are the contained ones.
        PreflightContract {
            contained: true,
            ..preflight_contract(&store, &registry, shape).unwrap()
        }
    }

    fn bug_shape() -> PreflightShape {
        PreflightShape {
            workflow: Some("bug".into()),
            repo_bound: true,
            council_size: Some(3),
            ..PreflightShape::default()
        }
    }

    fn preflight(
        clis: &[AgenticCli],
        shape: &PreflightShape,
        contract: PreflightContract,
    ) -> IndependencePreflight {
        on_container_floor(|| {
            independence_preflight_in(clis, &host(clis, &never_seen), shape, contract).unwrap()
        })
    }

    /// T-R2 (red: no API on main). One signed-in CLI, a repo-bound bug fix with a rule in force:
    /// every independent role is short, the rule council needs 3 if a rule is disputed, and the
    /// reason names the helper.
    #[test]
    fn t_r2_one_signed_in_cli_is_short_for_every_independent_role() {
        let shape = bug_shape();
        let contract = contract_for(&shape, true);
        assert_eq!(
            contract,
            PreflightContract {
                evaluator: true,
                judge: true,
                qe: true,
                team: true,
                rules: true,
                contained: true,
            },
            "a bug launch is a team run under the full contract"
        );
        let clis = [roster_seat("tr2", "claude")];
        let p = preflight(&clis, &shape, contract);
        assert!(p.short);
        assert_eq!(p.primary, None, "the launch draws its PA");
        for name in ["evaluator", "judge", "qe_reviewer"] {
            let r = role(&p, name);
            assert_eq!(
                (r.needed, r.short_by, r.conditional),
                (1, 1, false),
                "{name}"
            );
            assert!(r.real.is_empty(), "{name}: {:?}", r.real);
            assert_eq!(
                r.stand_in_from,
                Some(StandInFrom {
                    cli: "tr2-claude".into(),
                    models: vec![None]
                }),
                "{name}"
            );
        }
        let rule_council = role(&p, "rule_council");
        assert_eq!(
            (
                rule_council.needed,
                rule_council.short_by,
                rule_council.conditional
            ),
            (3, 3, true)
        );
        assert!(role(&p, "rule_judge").conditional);
        assert_eq!(role(&p, "council").short_by, 3);
        assert_eq!(p.reason.as_deref(), Some("only Claude Code was signed in"));
        assert_eq!(
            p.reason_data,
            Some(ShortReason::OnlyOneHelper {
                cli: "Claude Code".into()
            })
        );
        assert_eq!(p.signed_in, ["tr2-claude"]);
        let wire = serde_json::to_value(&p).unwrap();
        assert_eq!(
            wire["roles"][0]["shortBy"], 1,
            "camelCase on the wire: {wire}"
        );
        assert_eq!(wire["reasonData"]["kind"], "only_one_helper");
        assert!(wire["primary"].is_null(), "null, never absent: {wire}");
    }

    /// No rule in force: the rule roles are not asked about (`cli_runner.rs`: an empty rule set
    /// checks nothing).
    #[test]
    fn no_rule_in_force_needs_no_rule_judge_or_rule_council() {
        let shape = bug_shape();
        let contract = contract_for(&shape, false);
        assert!(!contract.rules);
        let p = preflight(&[roster_seat("tnr", "claude")], &shape, contract);
        assert_eq!(
            role_names(&p),
            ["evaluator", "judge", "qe_reviewer", "council", "monitor"]
        );
    }

    /// DES §2.2: two CLIs. On the expected shape Codex reviews and judges Claude Code's work, so
    /// those roles are real; a council has no non-party voter and the monitors want more seats.
    #[test]
    fn two_clis_are_real_for_review_and_short_for_a_council_and_monitors() {
        let clis = [roster_seat("tr22", "claude"), roster_seat("tr22", "codex")];
        let shape = PreflightShape {
            primary: Some("tr22-claude".into()),
            ..bug_shape()
        };
        let p = preflight(&clis, &shape, contract_for(&shape, true));
        assert_eq!(p.primary.as_deref(), Some("tr22-claude"));
        assert_eq!(
            role_names(&p),
            [
                "evaluator",
                "judge",
                "qe_reviewer",
                "rule_judge",
                "rule_council",
                "council",
                "monitor"
            ]
        );
        for name in ["evaluator", "judge", "qe_reviewer", "rule_judge"] {
            let r = role(&p, name);
            assert_eq!(r.real, ["tr22-codex"], "{name}");
            assert_eq!((r.short_by, r.why.as_deref()), (0, None), "{name}");
        }
        for name in ["council", "rule_council"] {
            let r = role(&p, name);
            assert!(r.real.is_empty() && r.conditional, "{name}");
            assert_eq!(r.short_by, 3, "{name}");
        }
        let target = usize::from(crate::team::supervisor::monitor_target(None, 0));
        let monitor = role(&p, "monitor");
        assert_eq!(monitor.real, ["tr22-codex"]);
        assert_eq!(monitor.short_by, target - 1);
        assert_eq!(
            p.reason_data,
            Some(ShortReason::HelpersShort {
                signed_in: 2,
                need: 3,
                have: 0
            })
        );
    }

    /// The other side of §3.9: a full roster is not asked. Five signed-in CLIs on a repo-bound bug
    /// fix with a rule in force, the PA drawn: every role has its real seats for every draw.
    #[test]
    fn five_signed_in_clis_are_not_short() {
        let clis: Vec<AgenticCli> = ["claude", "codex", "pi", "copilot", "opencode"]
            .into_iter()
            .map(|k| roster_seat("tr5", k))
            .collect();
        let shape = bug_shape();
        let p = preflight(&clis, &shape, contract_for(&shape, true));
        assert!(!p.short, "{:#?}", p.roles);
        assert_eq!((p.reason, p.reason_data), (None, None));
    }

    /// The launch's parties (review-change: who built the work) are never real seats, by key or
    /// by cli key, as the gate excludes a work author; a roster of nothing but parties says so.
    #[test]
    fn parties_are_never_real_seats_for_the_job() {
        let clis = [roster_seat("trp", "claude"), roster_seat("trp", "codex")];
        let shape = PreflightShape {
            parties: vec![" trp-claude#2 ".into(), "trp-codex".into()],
            ..PreflightShape::default()
        };
        let contract = PreflightContract {
            evaluator: true,
            ..PreflightContract::default()
        };
        let p = preflight(&clis, &shape, contract);
        assert!(role(&p, "evaluator").real.is_empty());
        assert_eq!(role(&p, "party_excluded").short_by, 1);
        assert_eq!(
            p.reason_data,
            Some(ShortReason::AllParties {
                seats: vec!["Claude Code".into(), "Codex".into()]
            })
        );
    }

    /// A party is named by the binary its record runs too: `claude` names a differently keyed seat
    /// that invokes Claude Code, as the gate's author exclusion resolves identities.
    #[test]
    fn a_party_names_every_seat_on_its_binary() {
        let clis = [roster_seat("tal", "claude"), roster_seat("tal", "codex")];
        let shape = PreflightShape {
            parties: vec!["claude".into()],
            ..PreflightShape::default()
        };
        let contract = PreflightContract {
            evaluator: true,
            ..PreflightContract::default()
        };
        let p = preflight(&clis, &shape, contract);
        assert_eq!(role(&p, "evaluator").short_by, 1, "{:?}", p.roles);
        assert_eq!(role(&p, "party_excluded").real, ["tal-codex"]);
    }

    /// The PA the launcher names is refused as the launch refuses it; `null` in the shape is the
    /// option's default, as at the launch boundary.
    #[test]
    fn a_primary_the_launch_refuses_is_refused_and_null_is_absent() {
        let clis = [roster_seat("trq", "claude")];
        let shape: PreflightShape = serde_json::from_str(
            r#"{"workflow":"bug","primary":"trq-codex","parties":null,"excludeSeats":null,
                "power":null,"reducedAssurance":null,"forceQeAcceptance":null,"repoBound":null}"#,
        )
        .unwrap();
        assert_eq!(shape.parties, Vec::<String>::new());
        let err = on_container_floor(|| {
            independence_preflight_in(
                &clis,
                &host(&clis, &never_seen),
                &shape,
                PreflightContract::default(),
            )
        })
        .unwrap_err();
        assert!(err.to_string().contains("not on the roster"), "{err}");
    }

    /// T-R3 (core#1721; red: no API on main). Pi was last seen calling Claude Code's provider, so
    /// three signed-in CLIs reach two providers; reviews need two plus one for a failover, and a
    /// registry helper running none of their CLIs is named. Observations are this test's own.
    #[test]
    fn t_r3_seats_seen_on_one_provider_count_once() {
        let registry = wicked_council::registry::builtin();
        let clis: Vec<AgenticCli> = ["claude", "pi", "codex"]
            .into_iter()
            .map(|k| {
                let mut c = registry.iter().find(|c| c.key == k).unwrap().clone();
                c.health = Some(wicked_council::types::SeatHealth {
                    usable: true,
                    reason: None,
                });
                c
            })
            .collect();
        let seen = |seat: &str| match seat {
            "claude" | "pi" => Some(vec!["api.anthropic.com:443".to_string()]),
            "codex" => Some(vec!["chatgpt.com:443".to_string()]),
            _ => None,
        };
        let contract = PreflightContract {
            evaluator: true,
            judge: true,
            qe: true,
            team: true,
            ..PreflightContract::default()
        };
        let host = HostFacts {
            registry: &registry,
            skills: None,
            seen: &seen,
        };
        let p = on_container_floor(|| {
            independence_preflight_in(&clis, &host, &PreflightShape::default(), contract).unwrap()
        });
        let d = &p.diversity;
        assert_eq!(d.providers, 2);
        assert_eq!((d.needed, d.needed_with_failover), (2, 3));
        assert_eq!(
            d.sharing,
            [ProviderSharing {
                seats: vec!["claude".into(), "pi".into()],
                provider: "api.anthropic.com:443".into()
            }]
        );
        let outside = d
            .outside_helper
            .as_deref()
            .expect("a helper outside the roster");
        assert!(
            !["Claude Code", "Pi CLI", "Codex"].contains(&outside),
            "{outside}"
        );
        let note = d.note.as_deref().unwrap();
        assert!(
            note.contains("claude and pi were seen calling api.anthropic.com:443")
                && note.contains("2 independent providers")
                && note.contains(&format!("Signing in {outside} could add another")),
            "{note}"
        );
        // QE admission and the judge rotation read the same observations: pi was seen on the
        // author's provider, so for a Claude Code PA only Codex can be the QE reviewer or judge.
        assert_eq!(role(&p, "qe_reviewer").real, ["codex"]);
        assert_eq!(role(&p, "judge").real, ["codex"]);
    }

    /// Seats seen on overlapping providers are not one group, and a seat never observed shares
    /// nothing it was not seen calling.
    #[test]
    fn overlap_is_not_a_shared_provider_and_unobserved_seats_share_nothing() {
        let clis = [
            roster_seat("tov", "claude"),
            roster_seat("tov", "codex"),
            roster_seat("tov", "pi"),
            roster_seat("tov", "opencode"),
        ];
        let seen = |seat: &str| match seat {
            "tov-claude" => Some(vec!["x:443".to_string()]),
            "tov-codex" => Some(vec!["x:443".to_string(), "y:443".to_string()]),
            "tov-pi" => Some(vec!["y:443".to_string()]),
            _ => None,
        };
        let p = on_container_floor(|| {
            independence_preflight_in(
                &clis,
                &host(&clis, &seen),
                &PreflightShape::default(),
                PreflightContract::default(),
            )
            .unwrap()
        });
        let d = &p.diversity;
        assert_eq!(d.providers, 3, "x, y and opencode's own");
        assert_eq!(
            d.sharing,
            [
                ProviderSharing {
                    seats: vec!["tov-claude".into(), "tov-codex".into()],
                    provider: "x:443".into()
                },
                ProviderSharing {
                    seats: vec!["tov-codex".into(), "tov-pi".into()],
                    provider: "y:443".into()
                }
            ]
        );
        // Two seats seen on the same providers are one independent provider, however many.
        let both = [roster_seat("tov2", "claude"), roster_seat("tov2", "codex")];
        let same = |_: &str| Some(vec!["x:443".to_string(), "y:443".to_string()]);
        let contract = PreflightContract {
            evaluator: true,
            ..PreflightContract::default()
        };
        let p = on_container_floor(|| {
            independence_preflight_in(
                &both,
                &host(&both, &same),
                &PreflightShape::default(),
                contract,
            )
            .unwrap()
        });
        assert_eq!(p.diversity.providers, 1);
        assert!(p.diversity.note.is_some());
    }

    /// The public seam: `Core::independence_preflight` resolves the contract on the actor (the
    /// boot-seeded presets) and refuses a primary the launch would refuse.
    #[test]
    fn core_independence_preflight_resolves_the_contract_on_the_actor() {
        struct NoCouncil;
        impl wicked_council::types::Dispatcher for NoCouncil {
            fn dispatch(
                &self,
                _: &AgenticCli,
                _: &wicked_council::CouncilTask,
            ) -> Option<wicked_council::types::Vote> {
                None
            }
        }
        let core = crate::Core::spawn_with_engine(
            ":memory:",
            std::sync::Arc::new(NoCouncil),
            std::sync::Arc::new(crate::workflow::StubStepRunner),
        );
        let clis = [roster_seat("trc", "claude")];
        let shape = PreflightShape {
            workflow: Some("bug".into()),
            ..PreflightShape::default()
        };
        let p = core.independence_preflight(&clis, &shape).unwrap();
        assert!(p.short);
        assert_eq!(role(&p, "evaluator").short_by, 1);
        assert!(role_names(&p).contains(&"council"), "bug is a team run");
        let err = core
            .independence_preflight(
                &clis,
                &PreflightShape {
                    primary: Some("nobody".into()),
                    ..shape
                },
            )
            .unwrap_err();
        assert!(err.to_string().contains("not on the roster"), "{err}");
        let bad = PreflightShape {
            plan_json: Some("{".into()),
            ..PreflightShape::default()
        };
        let err = core.independence_preflight(&clis, &bad).unwrap_err();
        assert!(
            err.to_string().contains("planJson is not a valid plan"),
            "{err}"
        );
    }
}
