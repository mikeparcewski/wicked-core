//! (core#850, codex audit EX-01..EX-05) The run's ASSURANCE CONTRACT and the receipt every gate
//! and delivery event carries.
//!
//! One contract per run, persisted on the session: the instruments the run REQUIRES (declared by
//! its workflow, else [`DEFAULT_REQUIRED`]) and its MODE — `full`, or `reduced` when the launch
//! explicitly opted in ([`crate::LaunchSpec::reduced_assurance`]). `reduced` waives exactly the two
//! seat-bound instruments ([`WAIVABLE`]) and is disclosed on every receipt; nothing else relaxes a
//! requirement. The engine enforces `distinct_evaluator` (distribution refuses a creator-seat
//! evaluator) and `judge` (a skipped required judge holds the gate); `qe_acceptance` is carried
//! for the launcher, whose delivery paths read the QE ledger.
//!
//! (QE-IN-APP-WORKFLOWS, operator ruling 2026-10-10) A run that requires `qe_acceptance` carries
//! its DECISION on the contract ([`RunAssurance::qe`]): `required`, `waived` (the run's impact
//! score is in the lowest band on every dimension — `review_scale::qe_waivable`) or `skipped`
//! (the operator said so at launch, with a reason). At launch it is provisional (`basis: plan`)
//! unless the operator decided (`basis: operator`: skip, or force); the BINDING decision is made
//! from the run's actual diff when its QE phase dispatches (`basis: diff`,
//! [`crate::qe_acceptance`]). Every receipt carries it, and a waiver or skip is a skipped
//! instrument with its reason — never silent.

use serde::{Deserialize, Serialize};

/// An evaluator unit never runs on the seat that built what it checks.
pub const DISTINCT_EVALUATOR: &str = "distinct_evaluator";
/// A pinned gate's semantic judge runs (on a seat distinct from the work's author).
pub const JUDGE: &str = "judge";
/// The launcher's QE acceptance verdict must be PASS before delivery (enforced by the launcher).
pub const QE_ACCEPTANCE: &str = "qe_acceptance";

/// Every token a workflow may declare in `required_instruments`.
pub const INSTRUMENTS: [&str; 3] = [DISTINCT_EVALUATOR, JUDGE, QE_ACCEPTANCE];
/// What a run requires when its workflow declares nothing.
pub const DEFAULT_REQUIRED: [&str; 2] = [DISTINCT_EVALUATOR, JUDGE];
/// What `reduced` waives.
pub const WAIVABLE: [&str; 2] = [DISTINCT_EVALUATOR, JUDGE];

/// Receipt instrument tokens (`ran` / `skipped[].instrument`) beyond the contract's own.
pub const PINNED_VALIDATOR: &str = "pinned_validator";
pub const REPO_CHECKS: &str = "repo_checks";
pub const EVALUATOR_PASS: &str = "evaluator_pass";
/// (operator ruling 2026-10-11) The operator judged the work at a `judge_unavailable` gate.
pub const OPERATOR_JUDGE: &str = "operator_judge";

/// (operator ruling 2026-10-11) "Judged by operator": the operator's explicit approve at a gate
/// whose judge could not rule. Labelled on every receipt, like reduced assurance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperatorJudgment {
    /// The actor the gate API recorded (the operator, or the agent acting for them).
    pub by: String,
    /// When, unix millis.
    pub at: i64,
    /// The operator's note, if any.
    #[serde(default)]
    pub note: Option<String>,
    /// Why the judge could not rule (the gate's denial reason) — or, on a floor override, why the
    /// floor denied.
    #[serde(default)]
    pub reason: Option<String>,
    /// (operator ruling 2026-10-21) Set when this judgment OVERRODE a failed repo-checks floor at
    /// its escalation gate (action [`crate::workflow::OPERATOR_JUDGMENT_ACTION`]): which floor
    /// result it overrode, labelled [`FLOOR_OVERRIDE_LABEL`]. `note` is then the operator's
    /// written rationale, never empty. Absent on a judge-unavailable judgment.
    #[serde(
        default,
        rename = "floorOverride",
        skip_serializing_if = "Option::is_none"
    )]
    pub floor_override: Option<FloorOverride>,
    /// (operator ruling 2026-10-20, core#1061 follow-on) Set when this judgment stood in for the
    /// JUDGE behind a floor override: the same operator judgment overrode a failed repo-checks
    /// floor whose gate also held a `judge_unavailable` no-verdict (no judge seat produced a
    /// verdict), and this record is the judge layer's — labelled [`JUDGE_STAND_IN_LABEL`]. `note`
    /// is the same rationale; `reason` is the judge layer's own denial text. Absent otherwise.
    #[serde(
        default,
        rename = "judgeStandIn",
        skip_serializing_if = "Option::is_none"
    )]
    pub judge_stand_in: Option<JudgeStandIn>,
}

/// (operator ruling 2026-10-21) The label every surface shows for a floor the operator overrode:
/// the run, the gate and the delivery.
pub const FLOOR_OVERRIDE_LABEL: &str = "floor overridden by operator";

/// (operator ruling 2026-10-20) The label of the judge layer an operator's floor override also
/// answered because no judge seat could rule ([`JudgeStandIn`]).
pub const JUDGE_STAND_IN_LABEL: &str = "judge: operator (no judge seat available)";

/// (operator ruling 2026-10-20: no judge available and no fallback judge ⇒ a person approves) The
/// judge layer an operator's judgment over a failed floor ALSO answered: the gate's only other
/// denial was the judge's `judge_unavailable` no-verdict, so the operator stands in for the judge
/// exactly as at a `judge_unavailable` gate of its own. Never a judge that RULED (a REJECT with a
/// verdict is `agent_validator` and refuses the override), and it waives nothing else: QE
/// acceptance and every later gate stand as they were.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeStandIn {
    /// The judge layer's denial source: [`crate::domain::DENIAL_SOURCE_JUDGE_UNAVAILABLE`].
    pub source: String,
    /// The floor result the same judgment overrode ([`FloorOverride::result_id`]).
    pub floor_result_id: String,
    /// [`JUDGE_STAND_IN_LABEL`], verbatim — the line to show as-is.
    pub label: String,
}

/// (operator ruling 2026-10-21) The failed floor result an operator's judgment overrode. The
/// override answers the deterministic repo-checks floor ONLY: every other layer of the gate still
/// denies (deny-dominates), and QE acceptance is untouched — it stays required where the run
/// requires it, and the delivery still needs its PASS.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FloorOverride {
    /// The floor layer that denied (`repo_checks` | `repo_checks_timeout`).
    pub layer: String,
    /// The floor result overridden: `<run>:unit-<ord>:attempt-<n>:<layer>` — the unit's
    /// `repoChecksEvaluated` frame for that attempt (or the carried result it stood on).
    pub result_id: String,
    /// The unit whose gate it was, and the attempt the floor denied.
    pub ord: u32,
    pub attempt: u32,
    /// The worktree tree the overridden checks ran on, when the report names it.
    #[serde(default)]
    pub tree: Option<String>,
    /// [`FLOOR_OVERRIDE_LABEL`], verbatim — the line to show as-is.
    pub label: String,
    /// (deliver honors an override) The overridden result's exact identity — tree, base and check
    /// set ([`crate::repo_checks::FloorKey`]). The deliver phase stands on this override instead
    /// of running the checks again only when the tree it ships, its base and its check set are
    /// all identical. `None` (an override recorded before the key existed, or over a floor that
    /// did not run on a known tree) is never stood on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<crate::repo_checks::FloorKey>,
    /// (core#1168) Set when this override was CARRIED, not given here: a verify floor that failed on
    /// exactly the result the operator overrode at the creator's floor (same tree and base, every
    /// failing check reused from it) stands on that override. The overridden result's id
    /// ([`FloorOverride::result_id`] of the original); `label` then reads
    /// [`carried_override_label`]. Absent on an override the operator gave at this gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_from: Option<String>,
}

/// (core#1168) The label of a carried override ([`FloorOverride::carried_from`]): the gate, its
/// receipt and the delivery show it as-is — "override carried from fix (unit 6), same tree".
pub fn carried_override_label(phase: &str, ord: u32) -> String {
    format!("override carried from {phase} (unit {ord}), same tree")
}

/// (operator ruling 2026-10-14) A judge seated IN PLACE of the run's preferred judge, because
/// every preferred judge seat was benched for the run (codex's worktree bench, a sign-in refusal)
/// or failed as a seat while judging. The fallback seat is drawn from the fallback seats (Claude
/// Code, then Antigravity — `cli_runner::JUDGE_FALLBACK_SEATS`), distinct from every seat the
/// contract excludes (the creator, every author, the evaluator, the validator author, the team).
/// (core#954) The same record discloses an evaluator unit's judge that is one of the work's
/// creators because no other eligible seat could judge: `preferred` is empty (there was no
/// distinct seat to stand in for) and the label reads `judge: <seat> (creator fallback; …)`.
/// Labelled on the gate, its receipt and the delivery receipt — never silent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JudgeFallback {
    /// The seat that judged.
    pub seat: String,
    /// The preferred judge seats it stood in for, in rotation order.
    pub preferred: Vec<String>,
    /// Why they could not judge, in the engine's words (`<seat> benched: <reason>` / `<seat>
    /// unavailable: <cause>`, `; `-joined).
    pub reason: String,
    /// The disclosure line: `judge: <seat> (fallback; <preferred> benched: <reason>)`.
    pub label: String,
}

/// Why an instrument is absent from a receipt.
pub const SKIP_REDUCED: &str = "reduced_assurance";
pub const SKIP_NO_DISTINCT_SEAT: &str = "no_distinct_seat";
pub const SKIP_NO_BOUNDARY: &str = "no_boundary";
pub const SKIP_ERROR: &str = "error";
pub const SKIP_NOT_APPLICABLE: &str = "not_applicable";

/// (QE acceptance) A waived or skipped QE acceptance on a receipt's `skipped[]`.
pub const SKIP_QE_WAIVED: &str = "qe_waived_by_score";
pub const SKIP_QE_OPERATOR: &str = "qe_skipped_by_operator";

/// [`QeAcceptance::status`].
pub const QE_REQUIRED: &str = "required";
pub const QE_WAIVED: &str = "waived";
pub const QE_SKIPPED: &str = "skipped";
/// [`QeAcceptance::basis`]: the launch's provisional decision, the operator's, or the diff's.
pub const QE_BASIS_PLAN: &str = "plan";
pub const QE_BASIS_OPERATOR: &str = "operator";
pub const QE_BASIS_DIFF: &str = "diff";

/// The operator's explicit word on QE acceptance at launch ([`crate::LaunchSpec::qe_acceptance`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum QeOverride {
    /// No word: the run's score decides at its QE phase.
    #[default]
    Auto,
    /// Skip it, for this reason (non-empty). Labelled on the run, every gate and the delivery.
    Skip(String),
    /// Require it whatever the score says.
    Force,
    /// The HOST's standing skip ([`HostQeAcceptance`]: "QE acceptance off"), on a launch the engine
    /// made on its own (a campaign node, a bus request) that carried no word of its own: skip, for
    /// this reason, ONLY when the run's contract requires QE acceptance; on any other run it is no
    /// word at all (a standing setting is never a refusal). Labelled exactly like [`Self::Skip`].
    Standing(String),
}

impl QeOverride {
    /// The word as every launch wire spells it (`LaunchOptions`, a campaign node's
    /// [`crate::campaign::RunSpec`], a bus `wicked.crew.run.requested`'s `args`): a skip reason, or
    /// a force, or neither. Both at once is refused (one word per launch).
    pub fn from_wire(skip_reason: Option<String>, force: bool) -> Result<Self, String> {
        match (skip_reason, force) {
            (Some(_), true) => Err(
                "skip_qe_acceptance_reason and force_qe_acceptance are mutually exclusive: skip \
                 QE acceptance with a reason, or force it"
                    .to_string(),
            ),
            (Some(reason), false) => Ok(Self::Skip(reason)),
            (None, true) => Ok(Self::Force),
            (None, false) => Ok(Self::Auto),
        }
    }
}

/// A launch refused because its QE word cannot apply ([`RunAssurance::with_qe_override`]: a blank
/// skip reason, or a skip or force on a run whose workflow does not require QE acceptance). Typed so
/// a caller that retries transient faults (the bus launch bridge) can tell a refusal that will never
/// succeed from one that might.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QeOverrideRefused(pub String);

impl std::fmt::Display for QeOverrideRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the launch is refused: {}", self.0)
    }
}

impl std::error::Error for QeOverrideRefused {}

/// The host's standing "QE acceptance off" word for the launches the ENGINE makes on its own —
/// campaign nodes and bus `wicked.crew.run.requested` requests — set by the host
/// ([`crate::Core::set_qe_acceptance_off`]; crew feeds its daemon setting into it). `Some(reason)`:
/// such a launch that carries no QE word of its own, and whose contract requires QE acceptance, is
/// skipped with `reason`, labelled exactly like an operator skip. `None` (the default): nothing is
/// skipped. Shared between the handle, the actor and the bus bridge thread; read at each launch.
#[derive(Debug, Clone, Default)]
pub struct HostQeAcceptance(std::sync::Arc<std::sync::RwLock<Option<String>>>);

/// The environment variable a host sets BEFORE `Core::spawn*` to start the engine with QE
/// acceptance already off for its own launches (the value is the reason). The bus bridge is armed
/// inside `spawn`, so a host that only called [`crate::Core::set_qe_acceptance_off`] afterwards
/// would leave a window in which a bus request launches under the default. Read once, at spawn;
/// the setter changes it from then on. Unset or blank: QE acceptance is required.
pub const QE_ACCEPTANCE_OFF_ENV: &str = "WICKED_QE_ACCEPTANCE_OFF";

impl HostQeAcceptance {
    /// The setting a `Core` starts with: [`QE_ACCEPTANCE_OFF_ENV`]'s reason when it is set and not
    /// blank, else off-nothing (QE acceptance required).
    pub fn from_env() -> Self {
        let host = Self::default();
        if let Some(reason) = std::env::var(QE_ACCEPTANCE_OFF_ENV)
            .ok()
            .filter(|r| !r.trim().is_empty())
        {
            let _ = host.set(Some(reason));
        }
        host
    }

    /// Turn QE acceptance off for engine-originated launches, with this reason (non-empty), or
    /// back on (`None`).
    pub fn set(&self, reason: Option<String>) -> Result<(), String> {
        let reason = match reason {
            Some(r) if r.trim().is_empty() => {
                return Err(
                    "turning QE acceptance off needs a reason (the reason is empty)".to_string(),
                )
            }
            Some(r) => Some(r.trim().to_string()),
            None => None,
        };
        *self
            .0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = reason;
        Ok(())
    }

    /// The word an engine-originated launch runs under: its own explicit word when it gave one,
    /// else the standing skip when QE acceptance is off, else [`QeOverride::Auto`].
    pub fn word_for(&self, explicit: QeOverride) -> QeOverride {
        match (explicit, self.get()) {
            (QeOverride::Auto, Some(reason)) => QeOverride::Standing(reason),
            (explicit, _) => explicit,
        }
    }

    /// The standing skip reason, when QE acceptance is off.
    pub fn get(&self) -> Option<String> {
        self.0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// A run's QE acceptance decision, on [`RunAssurance::qe`] and every receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QeAcceptance {
    /// [`QE_REQUIRED`] | [`QE_WAIVED`] | [`QE_SKIPPED`].
    pub status: String,
    /// [`QE_BASIS_PLAN`] | [`QE_BASIS_OPERATOR`] | [`QE_BASIS_DIFF`].
    pub basis: String,
    /// The impact score the decision read (`None` before the diff is scored, or for an operator
    /// decision).
    #[serde(default)]
    pub score: Option<u8>,
    /// The waiver line (`review_scale::THRESHOLDS.qe_waiver_max_score`).
    pub threshold: u8,
    /// The decision in words: what the plan, gate and delivery show.
    pub reason: String,
    /// The score's own lines (one per term), when it was scored.
    #[serde(default)]
    pub reasons: Vec<String>,
    /// The unit whose dispatch made a diff decision, and the tree it scored.
    #[serde(default)]
    pub ord: Option<u32>,
    #[serde(default)]
    pub tree: Option<String>,
}

impl QeAcceptance {
    pub fn required(&self) -> bool {
        self.status == QE_REQUIRED
    }
    /// The operator decided (skip or force): no score changes it.
    pub fn by_operator(&self) -> bool {
        self.basis == QE_BASIS_OPERATOR
    }
}

pub const MODE_FULL: &str = "full";
pub const MODE_REDUCED: &str = "reduced";

/// The run's contract, persisted on [`crate::domain::AgentSession::assurance`] and carried on
/// `sessionStarted.assurance`. A session persisted before this existed reads back as the
/// [`Default`]: `full` with [`DEFAULT_REQUIRED`] (fail-closed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAssurance {
    /// `full` | `reduced`.
    pub mode: String,
    /// The instruments the workflow requires (before any waiver), in [`INSTRUMENTS`] order.
    pub required: Vec<String>,
    /// (QE acceptance) The decision, present exactly when `required` holds `qe_acceptance` (a
    /// session from before the decision reads back `None`, which enforces it: fail-closed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qe: Option<QeAcceptance>,
    /// (operator ruling 2026-10-21) Every failed floor the operator overrode on this run, with who,
    /// when and why ([`OperatorJudgment::floor_override`] set) — the run's own
    /// "floor overridden by operator" label — each followed, when that gate's judge could not rule
    /// either, by the judge layer's record of the same judgment
    /// ([`OperatorJudgment::judge_stand_in`] set, operator ruling 2026-10-20). Absent when none
    /// (the routine case).
    #[serde(
        default,
        rename = "floorOverrides",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub floor_overrides: Vec<OperatorJudgment>,
}

impl Default for RunAssurance {
    fn default() -> Self {
        Self::new(None, false)
    }
}

impl RunAssurance {
    /// The contract for a workflow's declaration (`None` ⇒ [`DEFAULT_REQUIRED`]) and the launch's
    /// opt-in. Unknown tokens never reach here ([`validate_required`] refuses them at load); they
    /// are dropped defensively.
    pub fn new(declared: Option<&[String]>, reduced: bool) -> Self {
        let wanted: Vec<&str> = match declared {
            Some(d) => d.iter().map(String::as_str).collect(),
            None => DEFAULT_REQUIRED.to_vec(),
        };
        let required: Vec<String> = INSTRUMENTS
            .iter()
            .filter(|i| wanted.contains(i))
            .map(|i| i.to_string())
            .collect();
        let qe = required
            .iter()
            .any(|r| r == QE_ACCEPTANCE)
            .then(|| QeAcceptance {
                status: QE_REQUIRED.to_string(),
                basis: QE_BASIS_PLAN.to_string(),
                score: None,
                threshold: crate::review_scale::THRESHOLDS.qe_waiver_max_score,
                reason: PROVISIONAL_REQUIRED.to_string(),
                reasons: Vec::new(),
                ord: None,
                tree: None,
            });
        Self {
            mode: if reduced { MODE_REDUCED } else { MODE_FULL }.to_string(),
            required,
            qe,
            floor_overrides: Vec::new(),
        }
    }

    /// Apply the operator's launch-time word. A skip needs a non-empty reason; a skip or a force
    /// on a run whose workflow does not require QE acceptance is refused (there is nothing to
    /// skip, and nothing would run it).
    pub fn with_qe_override(mut self, over: &QeOverride) -> Result<Self, String> {
        if *over == QeOverride::Auto {
            return Ok(self);
        }
        // The host's standing skip applies only where there is something to skip.
        if matches!(over, QeOverride::Standing(_)) && self.qe.is_none() {
            return Ok(self);
        }
        let Some(qe) = self.qe.as_mut() else {
            return Err(format!(
                "this run's workflow does not require QE acceptance ({QE_ACCEPTANCE}), so there is \
                 nothing to {}",
                if matches!(over, QeOverride::Force) { "force" } else { "skip" }
            ));
        };
        qe.basis = QE_BASIS_OPERATOR.to_string();
        match over {
            QeOverride::Skip(reason) | QeOverride::Standing(reason) => {
                let reason = reason.trim();
                if reason.is_empty() {
                    return Err(
                        "skipping QE acceptance needs a reason (skipQeAcceptance.reason is empty)"
                            .to_string(),
                    );
                }
                qe.status = QE_SKIPPED.to_string();
                qe.reason = format!("QE acceptance skipped by operator: {reason}");
            }
            QeOverride::Force => {
                qe.status = QE_REQUIRED.to_string();
                qe.reason = "QE acceptance forced by operator: no score waives it".to_string();
            }
            QeOverride::Auto => unreachable!("returned above"),
        }
        Ok(self)
    }

    pub fn reduced(&self) -> bool {
        self.mode == MODE_REDUCED
    }

    /// Whether the run must have `instrument` — required AND not waived by `reduced` (and, for
    /// `qe_acceptance`, not waived or skipped by its decision).
    pub fn enforces(&self, instrument: &str) -> bool {
        self.required.iter().any(|r| r == instrument)
            && !(self.reduced() && WAIVABLE.contains(&instrument))
            && (instrument != QE_ACCEPTANCE || self.qe.as_ref().is_none_or(|q| q.required()))
    }
}

/// The launch's provisional reason: a plan has no diff, so the decision cannot waive yet.
pub const PROVISIONAL_REQUIRED: &str =
    "required (provisional): the binding decision is made from the run's diff when its QE phase \
     starts";

/// A workflow's `required_instruments`, judged at load: every token known, none repeated.
pub fn validate_required(declared: &[String]) -> Result<(), String> {
    for (i, t) in declared.iter().enumerate() {
        if !INSTRUMENTS.contains(&t.as_str()) {
            return Err(format!(
                "required_instruments: unknown instrument `{t}` (known: {})",
                INSTRUMENTS.join(", ")
            ));
        }
        if declared[..i].contains(t) {
            return Err(format!("required_instruments: `{t}` is listed twice"));
        }
    }
    Ok(())
}

/// One absent instrument and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedInstrument {
    pub instrument: String,
    pub reason: String,
    /// The engine's own words, when it has them (the judge-skip reason, the floor note); `null`
    /// otherwise.
    #[serde(default)]
    pub detail: Option<String>,
}

/// What assured THIS decision: the run's contract, the instruments that ran and the ones that did
/// not (and why), who made and judged the work, the tree, the attempt. On `gateEvaluated` and
/// `deliverLiftEvaluated`; persisted on the unit so a delivery receipt aggregates the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AssuranceReceipt {
    pub mode: String,
    pub required: Vec<String>,
    pub ran: Vec<String>,
    pub skipped: Vec<SkippedInstrument>,
    pub creator: Option<String>,
    pub evaluator: Option<String>,
    pub judge: Option<String>,
    pub tree: Option<String>,
    pub attempt: u32,
    /// (QE acceptance) The run's decision when this receipt was cut; absent when the run does not
    /// require QE acceptance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qe: Option<Box<QeAcceptance>>,
    /// (operator ruling 2026-10-11) The councils this unit's attempt convened on its disputes, in
    /// ledger order: each one's verdict and WHO decided — the seats drawn and the draw's seed.
    /// Absent when the attempt convened none (the routine case).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub councils: Vec<crate::team::Dispute>,
    /// (operator ruling 2026-10-11) "Judged by operator" — every gate the operator judged because
    /// no judge could rule; absent when none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_judges: Vec<OperatorJudgment>,
    /// (operator ruling 2026-10-14) Every judge seated as a FALLBACK for a benched or unavailable
    /// preferred judge ([`JudgeFallback`]); on a gate receipt at most one, on a delivery receipt
    /// every gate's. Absent when every judge was the run's own (the routine case).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub judge_fallbacks: Vec<JudgeFallback>,
}

impl AssuranceReceipt {
    pub fn for_run(run: &RunAssurance, attempt: u32) -> Self {
        let mut r = Self {
            mode: run.mode.clone(),
            required: run.required.clone(),
            attempt,
            qe: run.qe.clone().map(Box::new),
            ..Default::default()
        };
        // A waiver or a skip is a skipped instrument with its words — never silent.
        if let Some(q) = run.qe.as_ref() {
            match q.status.as_str() {
                QE_WAIVED => r.skip(QE_ACCEPTANCE, SKIP_QE_WAIVED, Some(q.reason.clone())),
                QE_SKIPPED => r.skip(QE_ACCEPTANCE, SKIP_QE_OPERATOR, Some(q.reason.clone())),
                _ => {}
            }
        }
        r
    }

    pub fn ran(&mut self, instrument: &str) {
        if !self.ran.iter().any(|r| r == instrument) {
            self.ran.push(instrument.to_string());
        }
    }

    pub fn skip(&mut self, instrument: &str, reason: &str, detail: Option<String>) {
        if !self.skipped.iter().any(|s| s.instrument == instrument) {
            self.skipped.push(SkippedInstrument {
                instrument: instrument.to_string(),
                reason: reason.to_string(),
                detail,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_contract_requires_a_distinct_evaluator_and_a_judge() {
        let full = RunAssurance::default();
        assert_eq!(full.mode, "full");
        assert_eq!(full.required, ["distinct_evaluator", "judge"]);
        assert!(full.enforces(DISTINCT_EVALUATOR) && full.enforces(JUDGE));
        assert!(!full.enforces(QE_ACCEPTANCE));
    }

    #[test]
    fn reduced_waives_only_the_seat_bound_instruments() {
        let declared = vec![QE_ACCEPTANCE.to_string(), JUDGE.to_string()];
        let r = RunAssurance::new(Some(&declared), true);
        assert_eq!(r.mode, "reduced");
        assert_eq!(
            r.required,
            ["judge", "qe_acceptance"],
            "in vocabulary order"
        );
        assert!(!r.enforces(JUDGE), "waived");
        assert!(r.enforces(QE_ACCEPTANCE), "never waived");
        assert!(!r.enforces(DISTINCT_EVALUATOR), "not required");
    }

    #[test]
    fn an_unknown_or_repeated_instrument_is_refused() {
        assert!(validate_required(&["judge".into()]).is_ok());
        assert!(validate_required(&[])
            .is_ok_and(|_| RunAssurance::new(Some(&[]), false).required.is_empty()));
        assert!(validate_required(&["vibes".into()])
            .unwrap_err()
            .contains("unknown instrument `vibes`"));
        assert!(validate_required(&["judge".into(), "judge".into()])
            .unwrap_err()
            .contains("twice"));
    }

    fn requiring_qe() -> RunAssurance {
        let declared = vec![JUDGE.to_string(), QE_ACCEPTANCE.to_string()];
        RunAssurance::new(Some(&declared), false)
    }

    #[test]
    fn a_contract_requiring_qe_acceptance_starts_provisionally_required() {
        let r = requiring_qe();
        let q = r.qe.as_ref().expect("a decision rides the requirement");
        assert_eq!(
            (q.status.as_str(), q.basis.as_str()),
            (QE_REQUIRED, QE_BASIS_PLAN)
        );
        assert_eq!(q.threshold, 20);
        assert!(r.enforces(QE_ACCEPTANCE));
        assert!(
            RunAssurance::default().qe.is_none(),
            "not required, no decision"
        );
        // Reduced assurance never waives it.
        let declared = vec![QE_ACCEPTANCE.to_string()];
        assert!(RunAssurance::new(Some(&declared), true).enforces(QE_ACCEPTANCE));
    }

    #[test]
    fn the_operator_skip_needs_a_reason_and_is_labelled_on_every_receipt() {
        assert!(requiring_qe()
            .with_qe_override(&QeOverride::Skip("  ".into()))
            .unwrap_err()
            .contains("needs a reason"));
        let r = requiring_qe()
            .with_qe_override(&QeOverride::Skip("hotfix, reviewed by hand".into()))
            .unwrap();
        assert!(!r.enforces(QE_ACCEPTANCE));
        let receipt = AssuranceReceipt::for_run(&r, 1);
        let q = receipt
            .qe
            .as_ref()
            .expect("the receipt carries the decision");
        assert_eq!(
            q.reason,
            "QE acceptance skipped by operator: hotfix, reviewed by hand"
        );
        assert_eq!(
            receipt.skipped,
            [SkippedInstrument {
                instrument: QE_ACCEPTANCE.into(),
                reason: SKIP_QE_OPERATOR.into(),
                detail: Some(q.reason.clone()),
            }]
        );
    }

    #[test]
    fn force_requires_and_a_word_on_a_run_without_the_requirement_is_refused() {
        let r = requiring_qe().with_qe_override(&QeOverride::Force).unwrap();
        let q = r.qe.as_ref().unwrap();
        assert_eq!(
            (q.status.as_str(), q.basis.as_str()),
            (QE_REQUIRED, QE_BASIS_OPERATOR)
        );
        assert!(r.enforces(QE_ACCEPTANCE));
        for over in [QeOverride::Force, QeOverride::Skip("x".into())] {
            assert!(RunAssurance::default()
                .with_qe_override(&over)
                .unwrap_err()
                .contains("does not require QE acceptance"));
        }
        assert_eq!(
            RunAssurance::default().with_qe_override(&QeOverride::Auto),
            Ok(RunAssurance::default())
        );
    }

    #[test]
    fn the_standing_skip_applies_only_where_qe_is_required() {
        let off = QeOverride::Standing("QE acceptance is off on this daemon".into());
        let r = requiring_qe().with_qe_override(&off).unwrap();
        let q = r.qe.as_ref().unwrap();
        assert_eq!(
            (q.status.as_str(), q.basis.as_str(), q.reason.as_str()),
            (
                QE_SKIPPED,
                QE_BASIS_OPERATOR,
                "QE acceptance skipped by operator: QE acceptance is off on this daemon"
            )
        );
        assert!(!r.enforces(QE_ACCEPTANCE));
        // Not required: untouched, never refused.
        assert_eq!(
            RunAssurance::default().with_qe_override(&off),
            Ok(RunAssurance::default())
        );
    }

    #[test]
    fn the_wire_word_and_the_host_setting_refuse_what_cannot_apply() {
        assert_eq!(QeOverride::from_wire(None, false), Ok(QeOverride::Auto));
        assert_eq!(QeOverride::from_wire(None, true), Ok(QeOverride::Force));
        assert_eq!(
            QeOverride::from_wire(Some("why".into()), false),
            Ok(QeOverride::Skip("why".into()))
        );
        assert!(QeOverride::from_wire(Some("why".into()), true)
            .unwrap_err()
            .contains("mutually exclusive"));
        let host = HostQeAcceptance::default();
        assert_eq!(host.get(), None);
        assert!(host
            .set(Some("  ".into()))
            .unwrap_err()
            .contains("needs a reason"));
        host.set(Some(" off here ".into())).unwrap();
        assert_eq!(host.clone().get().as_deref(), Some("off here"));
        // The launch's own word wins; no word reads the standing skip while it is set.
        host.set(Some("off".into())).unwrap();
        assert_eq!(host.word_for(QeOverride::Force), QeOverride::Force);
        assert_eq!(
            host.word_for(QeOverride::Skip("spike".into())),
            QeOverride::Skip("spike".into())
        );
        assert_eq!(
            host.word_for(QeOverride::Auto),
            QeOverride::Standing("off".into())
        );
        host.set(None).unwrap();
        assert_eq!(host.get(), None);
        assert_eq!(host.word_for(QeOverride::Auto), QeOverride::Auto);
    }

    #[test]
    fn a_waiver_is_a_skipped_instrument_with_its_words() {
        let mut r = requiring_qe();
        let q = r.qe.as_mut().unwrap();
        q.status = QE_WAIVED.into();
        q.basis = QE_BASIS_DIFF.into();
        q.reason = "waived: impact score 20 at or below the waiver line 20".into();
        assert!(!r.enforces(QE_ACCEPTANCE));
        let receipt = AssuranceReceipt::for_run(&r, 0);
        assert_eq!(receipt.skipped[0].reason, SKIP_QE_WAIVED);
        assert_eq!(
            receipt.skipped[0].detail.as_deref(),
            Some("waived: impact score 20 at or below the waiver line 20")
        );
    }

    #[test]
    fn a_legacy_session_reads_back_full() {
        let r: RunAssurance = serde_json::from_value(serde_json::json!({
            "mode": "full", "required": ["distinct_evaluator", "judge"]
        }))
        .unwrap();
        assert_eq!(r, RunAssurance::default());
    }
}
