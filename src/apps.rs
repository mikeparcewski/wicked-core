//! (DES-APPS-001 §9.2-§9.3, slice K0) The engine side of Apps: the two built-in presets an Apps
//! turn and an Apps publish run on, and the rules that hold for every run of them.
//!
//! An Apps turn is `build` (the creator edits `apps.json` and `queries/`) → `check` (crew's fixed
//! `wicked-crew apps-check`, a Tool: no agent code runs) → `review` (a seat distinct from the
//! builder judges the tree). A publish is `check` (with the manifest) → `qe` (forced QE acceptance
//! on a seat that built none of the turns since the last publish) → `produce` (crew's fixed
//! `wicked-crew apps-produce`, behind a `consent_before` gate). The steps are data
//! (`src/presets/apps-*.json`); THIS module is what makes them hold:
//!
//! - **Run identity** is the contract, never a def id: [`AppsContract`] rides
//!   [`crate::assurance::RunAssurance::apps`], persisted on the session at launch when the launch
//!   named an Apps preset and it resolved to the built-in. The names are reserved in every scope
//!   (`preset::put_preset`), and a stored non-built-in row of either name refuses the launch.
//! - **A fixed plan.** An Apps preset never goes through the plan gate (`plan_gate`): no PA scope
//!   step, no score floor fill, no re-plan, no edit at an approval gate. Any of those could drop
//!   the review (a creator landing its own turn) or add phases the design does not have, and an
//!   app repo with no code graph would score 100 and pause every turn. The preset composes
//!   straight over the catalog with its own instruments (`pipeline::resolve_workflow_def`).
//! - **Launch admission** ([`admit`]): the container floor, full assurance (no reduced mode, no
//!   power mode, no deliver step), a second eligible seat for the turn's review, and, for a
//!   publish, QE acceptance forced ([`contract`]) with every skip refused.
//! - **The QE builder exclusion** ([`exclusion_benches`]): a publish's launch `exclude_seats` names
//!   the seats that built a turn since the last publish (crew passes them). Each becomes a bench
//!   that never lifts, so no unit, QE reviewer, judge or failover of the run lands there; with
//!   every seat excluded the launch is refused, naming why.
//! - **No source tool for any agent** ([`RULE_NO_SOURCE_TOOL`]): every brokered MCP call and every
//!   tool listing of every unit of an Apps run is denied (`mcp_gate::evaluate`).
//! - **The publish QE backstop** ([`publish_consent_refusal`]): the produce step's consent gate
//!   cannot be approved unless the run's QE unit holds a bound QE PASS.

use serde::{Deserialize, Serialize};

use crate::assurance::{QeAcceptance, QeOverride, RunAssurance};
use crate::domain::{AgentSession, BenchedSeat, WorkUnit};

/// The turn preset: Build → Check → Review.
pub const APPS_TURN: &str = "apps-turn";
/// The publish preset: Check → QE (forced) → Produce (consent first).
pub const APPS_PUBLISH: &str = "apps-publish";
/// The garden skill every agent step of both presets runs.
pub const APPS_SKILL: &str = "wicked-garden-apps";
/// The step ids the presets fix (crew keys on them: the check token's unit, the QE unit, the
/// consent gate).
pub const BUILD_STEP: &str = "build";
pub const CHECK_STEP: &str = "check";
pub const REVIEW_STEP: &str = "review";
pub const QE_STEP: &str = "qe";
pub const PRODUCE_STEP: &str = "produce";
/// crew's CLI, which the check and produce Tools run.
pub const CREW_BIN: &str = "wicked-crew";
/// Where crew tells the engine its CLI is (crew under `npx` has no `wicked-crew` on PATH). Unset or
/// blank: `wicked-crew` from PATH.
pub const CREW_BIN_ENV: &str = "WICKED_CREW_BIN";
/// The engine's MCP deny for a unit of an Apps run: no registry tool, of any class, ever.
pub const RULE_NO_SOURCE_TOOL: &str = "engine:apps-no-source-tool";
/// `BenchedSeat::source` of the QE builder exclusion.
pub const EXCLUSION_BENCH_SOURCE: &str = "apps";
/// `BenchedSeat::reason` of the QE builder exclusion.
pub const EXCLUSION_BENCH_REASON: &str =
    "built an Apps turn since the last publish (the publish's QE seat must have built none)";
/// [`crate::assurance::QeAcceptance::reason`] of a publish's forced decision.
pub const QE_FORCED_REASON: &str =
    "required: QE acceptance forced by the apps-publish preset (no score, launch word or gate \
     skip waives it)";

/// (DES-APPS-001 §9.3) Whether phase `phase` of the def `def_id` is an Apps publish's QE step. The
/// def of a launch naming `apps-publish` is the built-in's, composed under that id
/// (`preset::compose_preset`); the name is reserved in every scope.
pub(crate) fn is_publish_qe_phase(def_id: &str, phase: &str) -> bool {
    def_id == APPS_PUBLISH && phase == QE_STEP
}

/// Whether `name` is one of the two Apps presets.
pub fn is_apps_preset(name: &str) -> bool {
    name == APPS_TURN || name == APPS_PUBLISH
}

/// The Apps part of a run's contract ([`RunAssurance::apps`]): which preset the run is. Present
/// exactly when the launch named an Apps preset and it resolved to the built-in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppsContract {
    /// [`APPS_TURN`] | [`APPS_PUBLISH`].
    pub preset: String,
}

impl AppsContract {
    pub fn publish(&self) -> bool {
        self.preset == APPS_PUBLISH
    }
}

/// The launch facts admission judges, as every launch route spells them.
pub(crate) struct LaunchFacts<'a> {
    pub workflow: Option<&'a str>,
    pub project_id: Option<&'a str>,
    pub plan: bool,
    pub reduced_assurance: bool,
    pub power: bool,
    pub deliver_step: bool,
    pub qe: &'a QeOverride,
    pub clis: &'a [crate::AgenticCli],
    pub exclude_seats: &'a [String],
    /// `WICKED_SEAT_FLOOR` as the process holds it.
    pub floor: Option<&'a str>,
}

impl<'a> LaunchFacts<'a> {
    pub(crate) fn of(spec: &'a crate::LaunchSpec, floor: Option<&'a str>) -> Self {
        LaunchFacts {
            workflow: spec.workflow.as_deref(),
            project_id: spec.project_id.as_deref(),
            plan: spec.plan.is_some(),
            reduced_assurance: spec.reduced_assurance,
            power: spec.power,
            deliver_step: spec.deliver_step.is_some(),
            qe: &spec.qe_acceptance,
            clis: &spec.clis,
            exclude_seats: &spec.exclude_seats,
            floor,
        }
    }
}

/// The floor in force (`WICKED_SEAT_FLOOR`).
pub(crate) fn floor_env() -> Option<String> {
    std::env::var(crate::container_floor::SEAT_FLOOR_ENV).ok()
}

/// The refusal when the floor in force is not the container floor (`None` = it is).
pub(crate) fn floor_refusal(preset: &str, floor: Option<&str>) -> Option<String> {
    (!crate::container_floor::container_mode_from(floor)).then(|| {
        format!(
            "`{preset}` runs only on the container floor (WICKED_SEAT_FLOOR=container), and the \
             floor in force is {}: an Apps seat must reach crew only through its gateway's MCP \
             routes, which the host floor does not guarantee",
            match floor.map(str::trim).filter(|f| !f.is_empty()) {
                Some(f) => format!("`{f}`"),
                None => "unset".to_string(),
            }
        )
    })
}

/// (Launch admission) The Apps contract of a launch, or `None` when it names no Apps preset;
/// `Err` (nothing persisted) when it does and may not run. Every launch route calls this before
/// it writes anything.
pub(crate) fn admit(
    store: &dyn wicked_apps_core::GraphRead,
    f: &LaunchFacts<'_>,
) -> anyhow::Result<Option<AppsContract>> {
    let Some(name) = f.workflow.filter(|w| is_apps_preset(w)) else {
        return Ok(None);
    };
    let refuse = |why: String| anyhow::anyhow!("the `{name}` launch is refused: {why}");
    match crate::preset::resolve(store, f.project_id, name)? {
        Some(p) if p.created_by == crate::preset::BUILTIN_CREATED_BY => {}
        Some(p) => {
            return Err(refuse(format!(
                "a saved preset (`{}`, scope {}) holds the reserved name; delete it to launch the \
                 built-in",
                p.created_by, p.scope
            )))
        }
        None => {
            return Err(refuse(
                "the built-in preset is not seeded in this store".into(),
            ))
        }
    }
    if f.plan {
        return Err(refuse("an Apps run carries no plan of its own".into()));
    }
    if let Some(why) = floor_refusal(name, f.floor) {
        return Err(refuse(why));
    }
    if f.reduced_assurance {
        return Err(refuse(
            "an Apps run cannot be launched in reduced-assurance mode: every turn is reviewed by \
             a seat distinct from its builder, and a publish needs QE acceptance"
                .into(),
        ));
    }
    if f.power {
        return Err(refuse(
            "an Apps run cannot be launched in power mode".into(),
        ));
    }
    if f.deliver_step {
        return Err(refuse(
            "an Apps run carries no deliver step (crew lands the turn or the publish itself)"
                .into(),
        ));
    }
    let contract = AppsContract {
        preset: name.to_string(),
    };
    if contract.publish() {
        if let QeOverride::Skip(_) = f.qe {
            return Err(refuse(
                "QE acceptance cannot be skipped on a publish (it is forced by the preset)".into(),
            ));
        }
    } else if !matches!(f.qe, QeOverride::Auto | QeOverride::Standing(_)) {
        return Err(refuse(
            "an Apps turn has no QE acceptance to skip or force (its review is the distinct seat)"
                .into(),
        ));
    }
    // The seats the run could seat, as distribution will see them: the roster's work seats minus
    // the launcher's unusable ones, minus (on a publish) the builder exclusion.
    let mut benched = crate::distribute::launcher_benched(f.clis);
    for b in exclusion_benches(&contract, f.exclude_seats) {
        crate::domain::bench_seat(&mut benched, b);
    }
    let eligible = crate::distribute::eligible_seats(f.clis, &benched).len();
    if contract.publish() && eligible == 0 && !f.clis.is_empty() {
        return Err(anyhow::Error::new(crate::actor::NoEligibleSeat {
            run_id: name.to_string(),
            benched: crate::domain::benched_summary(&benched, f.clis.len()).unwrap_or_default(),
            benched_seats: benched,
        })
        .context(refuse(
            "every seat built a turn since the last publish, and the QE seat must have built none \
             — add a seat that did not build, then publish"
                .into(),
        )));
    }
    if !contract.publish() && eligible < 2 {
        return Err(refuse(format!(
            "a turn is reviewed by a seat distinct from its builder, and this roster has {eligible} \
             usable seat{} — add a seat, or edit the app directly",
            if eligible == 1 { "" } else { "s" }
        )));
    }
    Ok(Some(contract))
}

/// The Apps contract a session is built with, from the launch's `workflow` alone (the pipeline
/// path, which builds a session no stub wrote): the built-in Apps preset it names, else `None`.
/// [`admit`] has already refused every launch this would mis-read.
pub(crate) fn contract_for(
    store: &dyn wicked_apps_core::GraphRead,
    project_id: Option<&str>,
    workflow: Option<&str>,
) -> anyhow::Result<Option<AppsContract>> {
    let Some(name) = workflow.filter(|w| is_apps_preset(w)) else {
        return Ok(None);
    };
    Ok(crate::preset::resolve(store, project_id, name)?
        .filter(|p| p.created_by == crate::preset::BUILTIN_CREATED_BY)
        .map(|_| AppsContract {
            preset: name.to_string(),
        }))
}

/// The run's contract with its launch QE word applied: [`RunAssurance::with_qe_override`] for
/// every run but an Apps one. An Apps publish's QE decision is FORCED (basis
/// [`crate::assurance::QE_BASIS_PRESET`], never re-scored), whatever the word: an explicit skip is
/// refused, the host's standing skip does not apply. An Apps turn takes no word but the host's
/// standing one, which has nothing to skip there.
pub(crate) fn contract(
    run: RunAssurance,
    apps: Option<AppsContract>,
    over: &QeOverride,
) -> Result<RunAssurance, String> {
    let Some(apps) = apps else {
        return run.with_qe_override(over);
    };
    let mut run = run;
    if apps.publish() {
        if let QeOverride::Skip(_) = over {
            return Err(
                "QE acceptance cannot be skipped on an Apps publish (it is forced by the preset)"
                    .to_string(),
            );
        }
        let Some(qe) = run.qe.as_mut() else {
            return Err(
                "the apps-publish contract does not require QE acceptance (its instruments did \
                 not resolve)"
                    .to_string(),
            );
        };
        *qe = QeAcceptance {
            status: crate::assurance::QE_REQUIRED.to_string(),
            basis: crate::assurance::QE_BASIS_PRESET.to_string(),
            reason: QE_FORCED_REASON.to_string(),
            ..qe.clone()
        };
    } else if !matches!(over, QeOverride::Auto | QeOverride::Standing(_)) {
        return Err(
            "an Apps turn has no QE acceptance to skip or force (its review is the distinct seat)"
                .to_string(),
        );
    }
    run.apps = Some(apps);
    Ok(run)
}

/// (QE builder exclusion) The benches a publish's launch exclusion becomes: one per named seat,
/// never lifting by itself (`seat_bench` re-probes quota benches only). Empty on a turn.
pub(crate) fn exclusion_benches(apps: &AppsContract, exclude_seats: &[String]) -> Vec<BenchedSeat> {
    if !apps.publish() {
        return Vec::new();
    }
    crate::domain::normalize_exclude_seats(exclude_seats)
        .into_iter()
        .map(|cli| BenchedSeat {
            cli,
            reason: EXCLUSION_BENCH_REASON.to_string(),
            source: EXCLUSION_BENCH_SOURCE.to_string(),
            governed_only: false,
            benched_until: None,
            strikes: 0,
            replaced: None,
        })
        .collect()
}

/// The launch bench with the publish's builder exclusion FIRST, so a later bench of another
/// cause never replaces it (`domain::bench_seat` keeps the first non-governed-only bench).
pub(crate) fn with_exclusion_benches(
    apps: Option<&AppsContract>,
    exclude_seats: &[String],
    bench: Vec<BenchedSeat>,
) -> Vec<BenchedSeat> {
    let Some(apps) = apps else {
        return bench;
    };
    let mut out = exclusion_benches(apps, exclude_seats);
    for b in bench {
        crate::domain::bench_seat(&mut out, b);
    }
    out
}

/// The Apps preset a session's contract names, when it is an Apps run.
pub(crate) fn run_preset(session: &AgentSession) -> Option<&str> {
    session.assurance.apps.as_ref().map(|a| a.preset.as_str())
}

/// Whether `session` is an Apps publish.
pub(crate) fn is_publish(session: &AgentSession) -> bool {
    session
        .assurance
        .apps
        .as_ref()
        .is_some_and(AppsContract::publish)
}

/// The argv a Tool unit of an Apps run spawns: `wicked-crew` resolved through [`CREW_BIN_ENV`]
/// (crew under `npx` has no `wicked-crew` on PATH), every other argv as written.
pub(crate) fn resolve_tool_cmd(cmd: &[String], crew_bin: Option<&str>) -> Vec<String> {
    let mut out = cmd.to_vec();
    if let (Some(first), Some(bin)) = (
        out.first_mut(),
        crew_bin.map(str::trim).filter(|b| !b.is_empty()),
    ) {
        if first == CREW_BIN {
            *first = bin.to_string();
        }
    }
    out
}

/// (The publish QE backstop) Why the consent gate before an Apps publish's produce step may not be
/// approved, or `None` when it may: the run's QE decision is required (never skipped or waived)
/// and its QE unit holds a bound QE PASS. Every path that could let the QE unit count without one
/// — an operator's skip, an operator's judgment at a gate no reviewer could rule, a floor
/// override — ends here: produce runs only after the consent gate.
pub(crate) fn publish_consent_refusal(
    session: &AgentSession,
    units: &[WorkUnit],
) -> Option<String> {
    if !is_publish(session) {
        return None;
    }
    let required = session
        .assurance
        .qe
        .as_ref()
        .is_some_and(QeAcceptance::required);
    if !required {
        return Some(format!(
            "an Apps publish produces only after QE acceptance, and this run's QE decision is {}",
            session
                .assurance
                .qe
                .as_ref()
                .map_or("absent".to_string(), |q| format!(
                    "{} ({})",
                    q.status, q.reason
                ))
        ));
    }
    let qe = units
        .iter()
        .find(|u| crate::qe_acceptance::is_qe_unit(u) && u.tool_cmd.is_none());
    let pass = qe.and_then(|u| u.qe_review.as_ref()).is_some_and(|r| {
        r.outcome == crate::qe_acceptance::review::OUTCOME_PASS && r.verdict.is_some()
    });
    (!pass).then(|| {
        format!(
            "an Apps publish produces only after a bound QE PASS, and {}",
            match qe {
                None => "the run has no QE unit".to_string(),
                Some(u) => match u.qe_review.as_ref() {
                    None => format!("QE unit {} has no reviewer record", u.ord),
                    Some(r) => format!("QE unit {}'s reviewer record is `{}`", u.ord, r.outcome),
                },
            }
        )
    })
}

#[cfg(test)]
mod tests;
