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
    /// The launch's run id (named on a typed refusal).
    pub run_id: &'a str,
    pub workflow: Option<&'a str>,
    pub project_id: Option<&'a str>,
    /// The launch names a registered repository (the app's repo).
    pub repo: bool,
    /// The launch minted an evidence root (where a publish's QE verdict is recorded).
    pub evidence_root: bool,
    /// Input governance is armed on this engine (`actor::in_process_governance`): without it no
    /// unit carries the registry-tool ban and no seat is contained.
    pub governed: bool,
    /// The daemon has a broker URL (`WICKED_CREW_URL`): the check and produce Tools reach crew's
    /// route with their unit's token through it.
    pub crew_url: bool,
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
    /// The facts of `spec` on this engine: `floor` is `WICKED_SEAT_FLOOR`, `governed` whether input
    /// governance is armed on the calling (actor) thread.
    pub(crate) fn of(spec: &'a crate::LaunchSpec, floor: Option<&'a str>, governed: bool) -> Self {
        LaunchFacts {
            run_id: &spec.session_id,
            workflow: spec.workflow.as_deref(),
            project_id: spec.project_id.as_deref(),
            repo: spec.repo_ref.is_some(),
            evidence_root: spec
                .evidence_root
                .as_deref()
                .is_some_and(|r| !r.trim().is_empty()),
            governed,
            crew_url: crate::mcp_gate::daemon_crew_url().is_some_and(|u| !u.trim().is_empty()),
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

/// A launch of an Apps preset that may not run, refused before anything is persisted. Typed, so a
/// caller that retries transient faults (the bus launch bridge) drops it: the same launch is
/// refused the same way every time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppsLaunchRefused(pub String);

impl std::fmt::Display for AppsLaunchRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AppsLaunchRefused {}

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
    let refuse = |why: String| {
        anyhow::Error::new(AppsLaunchRefused(format!(
            "the `{name}` launch is refused: {why}"
        )))
    };
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
    if !f.governed {
        return Err(refuse(
            "input governance is not armed on this engine (a file-backed store), so no unit could \
             carry the registry-tool ban or run contained"
                .into(),
        ));
    }
    if !f.crew_url {
        return Err(refuse(
            "the daemon has no broker URL (WICKED_CREW_URL), so crew's check could not be called \
             with its unit's token"
                .into(),
        ));
    }
    if !f.repo {
        return Err(refuse(
            "an Apps run works in its app's registered repository, and this launch names none"
                .into(),
        ));
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
        if !f.evidence_root {
            return Err(refuse(
                "a publish records its QE verdict under the run's evidence root, and this launch \
                 minted none"
                    .into(),
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
    let mut benched = exclusion_benches(&contract, f.exclude_seats, f.clis);
    for b in crate::distribute::launcher_benched(f.clis) {
        crate::domain::bench_seat(&mut benched, b);
    }
    let eligible = crate::distribute::eligible_seats(f.clis, &benched).len();
    if contract.publish() && eligible == 0 {
        return Err(anyhow::Error::new(crate::actor::NoEligibleSeat {
            run_id: f.run_id.to_string(),
            benched: crate::domain::benched_summary(&benched, f.clis.len()).unwrap_or_default(),
            benched_seats: benched,
        })
        .context(format!(
            "the `{name}` launch is refused: every seat built a turn since the last publish, and \
             the QE seats must have built none — add seats that did not build, then publish"
        )));
    }
    // A publish's QE runs on one seat and its evidence is reviewed, in isolation, on another
    // (`qe_acceptance::review`); a turn's review is a seat distinct from its builder.
    if eligible < 2 {
        return Err(refuse(if contract.publish() {
            "a publish's QE runs on one seat that built none of the turns and is reviewed on \
             another, and this roster has 1 such seat — add a seat"
                .to_string()
        } else {
            format!(
                "a turn is reviewed by a seat distinct from its builder, and this roster has \
                 {eligible} usable seat{} — add a seat, or edit the app directly",
                if eligible == 1 { "" } else { "s" }
            )
        }));
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

/// (QE builder exclusion) The benches a publish's launch exclusion becomes: one per roster seat it
/// names — by seat key, or by CLI key for every instance of that CLI, the rule the QE reviewer's
/// seating reads (`qe_acceptance::review::candidates`) — never lifting by itself (`seat_bench`
/// re-probes quota benches only). Empty on a turn.
pub(crate) fn exclusion_benches(
    apps: &AppsContract,
    exclude_seats: &[String],
    roster: &[crate::AgenticCli],
) -> Vec<BenchedSeat> {
    if !apps.publish() {
        return Vec::new();
    }
    let excluded = crate::domain::normalize_exclude_seats(exclude_seats);
    roster
        .iter()
        .filter(|c| {
            excluded
                .iter()
                .any(|e| *e == c.key || e == crate::team::runner::seat_key(&c.key))
        })
        .map(|c| BenchedSeat {
            cli: c.key.clone(),
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
    roster: &[crate::AgenticCli],
    bench: Vec<BenchedSeat>,
) -> Vec<BenchedSeat> {
    let Some(apps) = apps else {
        return bench;
    };
    let mut out = exclusion_benches(apps, exclude_seats, roster);
    for b in bench {
        crate::domain::bench_seat(&mut out, b);
    }
    out
}

/// The benches no operator lever lifts: a publish's builder exclusion. An operator's re-route
/// (`{cli: null}`) runs over a cleared bench so a seat signed in since is seated again; these
/// stay, or the re-route would seat the publish's QE on a seat that built a turn.
pub(crate) fn binding_benches(bench: &[BenchedSeat]) -> Vec<BenchedSeat> {
    bench
        .iter()
        .filter(|b| b.source == EXCLUSION_BENCH_SOURCE)
        .cloned()
        .collect()
}

/// Why an operator's reassign of a unit of `session` to `cli` is refused, or `None`: a publish's
/// excluded builder seat takes no unit of it, however it is asked.
pub(crate) fn reassign_refusal(session: &AgentSession, cli: &str) -> Option<String> {
    // By seat key, as distribution compares benches (`distribute::eligible_seats`).
    binding_benches(&session.benched_seats)
        .iter()
        .any(|b| b.cli == cli)
        .then(|| {
            format!(
                "seat '{cli}' {EXCLUSION_BENCH_REASON}, so no unit of this publish runs there — \
                 reassign to a seat that built none of this app's turns, or stop the run"
            )
        })
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

/// The binary a Tool command's `wicked-crew` runs: `WICKED_CREW_BIN` when crew set it (crew under
/// `npx` has no `wicked-crew` on PATH), else `wicked-crew` from PATH. The one resolver the launch
/// preflight and the Tool spawn both read, as `wicked-core` has its own.
pub(crate) fn crew_bin_from(env: Option<&str>) -> String {
    env.map(str::trim)
        .filter(|b| !b.is_empty())
        .unwrap_or(CREW_BIN)
        .to_string()
}

/// [`crew_bin_from`] the process's [`CREW_BIN_ENV`].
pub(crate) fn crew_bin() -> String {
    crew_bin_from(std::env::var(CREW_BIN_ENV).ok().as_deref())
}

/// (The publish QE backstop) Why an Apps publish may not produce, or `None` when it may: the run's
/// QE decision is required (never skipped or waived), and its QE unit holds a bound QE PASS that an
/// isolated reviewer on a seat distinct from the QE executor recorded, taken on the tree the run
/// holds now (`current_tree`; `None` = unreadable, refused). Checked at the produce step's consent
/// gate and again when the produce step dispatches, so no answer, skip, judgment or override lets
/// produce run without one.
pub(crate) fn publish_consent_refusal(
    session: &AgentSession,
    units: &[WorkUnit],
    current_tree: Option<&str>,
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
    let Some(qe) = units
        .iter()
        .find(|u| crate::qe_acceptance::is_qe_unit(u) && u.tool_cmd.is_none())
    else {
        return Some(
            "an Apps publish produces only after QE acceptance, and the run has no QE unit".into(),
        );
    };
    let Some(r) = qe.qe_review.as_ref() else {
        return Some(format!(
            "an Apps publish produces only after a bound QE PASS, and QE unit {} has no reviewer \
             record",
            qe.ord
        ));
    };
    if r.outcome != crate::qe_acceptance::review::OUTCOME_PASS || r.verdict.is_none() {
        return Some(format!(
            "an Apps publish produces only after a bound QE PASS, and QE unit {}'s reviewer \
             record is `{}`",
            qe.ord, r.outcome
        ));
    }
    // The verdict's reviewer: the last turn that recorded one, on a seat distinct from the
    // executor — never the executor's own seat standing in (`BASIS_EXECUTOR`).
    let distinct = r
        .attempts
        .iter()
        .rev()
        .find(|a| a.status == "verdict")
        .is_some_and(|a| {
            a.basis == crate::qe_acceptance::review::BASIS_DISTINCT
                && qe.assigned_cli.as_deref() != Some(a.seat.as_str())
        });
    if !distinct {
        return Some(format!(
            "an Apps publish produces only after its QE evidence was reviewed on a seat distinct \
             from the QE executor, and QE unit {}'s PASS was not",
            qe.ord
        ));
    }
    match (r.tree.as_deref(), current_tree) {
        (Some(judged), Some(now)) if judged == now => None,
        (judged, now) => Some(format!(
            "an Apps publish produces exactly the tree its QE accepted, and QE unit {} accepted {} \
             while the run holds {}",
            qe.ord,
            judged.unwrap_or("no recorded tree"),
            now.unwrap_or("an unreadable tree")
        )),
    }
}

/// (DES-APPS-001 K0) Why the engine refuses to dispatch `unit` of an Apps run, or `None`. Every
/// path that reaches a unit (a launch, an approve, a retry, a reassign, a resume after a restart)
/// dispatches through here:
/// - an agent unit runs only on the container floor (`floor`), with input governance armed
///   (`governed`: the registry-tool ban rides it);
/// - no unit of a publish runs on a seat its builder exclusion benched;
/// - a publish's produce step (a Tool after its QE unit) runs only past [`publish_consent_refusal`].
pub(crate) fn dispatch_refusal(
    session: &AgentSession,
    units: &[WorkUnit],
    unit: &WorkUnit,
    floor: Option<&str>,
    governed: bool,
    current_tree: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let preset = run_preset(session)?;
    if unit.tool_cmd.is_none() {
        if let Some(why) = floor_refusal(preset, floor) {
            return Some(why);
        }
        if !governed {
            return Some(format!(
                "`{preset}` runs its agent units only with input governance armed (the \
                 registry-tool ban rides it), and this engine has none"
            ));
        }
        if let Some(seat) = unit.assigned_cli.as_deref() {
            if let Some(why) = reassign_refusal(session, seat) {
                return Some(why);
            }
        }
        return None;
    }
    let after_qe = units
        .iter()
        .any(|u| crate::qe_acceptance::is_qe_unit(u) && u.tool_cmd.is_none() && u.ord < unit.ord);
    if is_publish(session) && after_qe {
        return publish_consent_refusal(session, units, current_tree().as_deref());
    }
    None
}

#[cfg(test)]
mod tests;
