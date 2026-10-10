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

/// Why an instrument is absent from a receipt.
pub const SKIP_REDUCED: &str = "reduced_assurance";
pub const SKIP_NO_DISTINCT_SEAT: &str = "no_distinct_seat";
pub const SKIP_NO_BOUNDARY: &str = "no_boundary";
pub const SKIP_ERROR: &str = "error";
pub const SKIP_NOT_APPLICABLE: &str = "not_applicable";

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
        Self {
            mode: if reduced { MODE_REDUCED } else { MODE_FULL }.to_string(),
            required: INSTRUMENTS
                .iter()
                .filter(|i| wanted.contains(i))
                .map(|i| i.to_string())
                .collect(),
        }
    }

    pub fn reduced(&self) -> bool {
        self.mode == MODE_REDUCED
    }

    /// Whether the run must have `instrument` — required AND not waived by `reduced`.
    pub fn enforces(&self, instrument: &str) -> bool {
        self.required.iter().any(|r| r == instrument)
            && !(self.reduced() && WAIVABLE.contains(&instrument))
    }
}

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
}

impl AssuranceReceipt {
    pub fn for_run(run: &RunAssurance, attempt: u32) -> Self {
        Self {
            mode: run.mode.clone(),
            required: run.required.clone(),
            attempt,
            ..Default::default()
        }
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

    #[test]
    fn a_legacy_session_reads_back_full() {
        let r: RunAssurance = serde_json::from_value(serde_json::json!({
            "mode": "full", "required": ["distinct_evaluator", "judge"]
        }))
        .unwrap();
        assert_eq!(r, RunAssurance::default());
    }
}
