//! The phase catalog (DES-TEAMING-002 §8.3): the one place every phase TYPE a plan may use is
//! defined. Data and nothing else — no workflow file, no per-surface copy.
//!
//! Each entry IS a [`PhaseDef`], so every existing control keeps working through the mechanism
//! that already enforces it: `plan_from_def` copies `gate` / `role` / `owner` onto the unit,
//! `attach_pinned_validators` attaches each `validator_pin`, and the fence reads `role`. A plan's
//! steps are composed onto these entries by [`crate::plan::compose`], which lets a step only make
//! its entry STRICTER (raise the gate, add or swap a pin, set `executes_code`) and never touch
//! `role`.
//!
//! The evidence floor lives HERE (DES-TEAMING-002 §10, "moved in seam C1"): the `build`, `test`,
//! `review` and `security_review` entries carry [`EVIDENCE_FLOOR_PIN`] as data, so a composed
//! plan's code-writing and code-judging steps are floored by the catalog, not by each def.
//!
//! Deviations from the §8.3 table, each forced by "compose of today's def equals today's def"
//! (seam C1 acceptance (a)) and recorded in the C1 PR:
//! - `run` has no single kind in the table ("per step"); the entry's default is `recon`, the kind
//!   today's tool phases mostly carry, and a `run` step may set its own.
//! - `deliver` is `executes_code: false` (the table says `true`): crew's composed deliver phase
//!   (`deliverPrPhase`, crew `packages/crew/src/core/deliver.ts:798`) is an engine-owned Tool
//!   phase with `executes_code: false`, and a step may never lower `executes_code`.
//! - `Tool` entries (`run`, `deliver`) carry an EMPTY command: the step supplies it, and
//!   `compose` refuses a Tool step that does not.

use std::sync::OnceLock;

use crate::builtin_floors::EVIDENCE_FLOOR_PIN;
use crate::domain::StageKind;
use crate::workflow::{
    GateCond, GateSpec, GateType, PhaseDef, PhaseExecutor, PhaseRole, StepOwner,
};

/// The garden QE security specialist `security_review` runs (DES-TEAMING-002 Q6, fixed in C1): the
/// frontmatter name of `skills/qe-security-test-engineer/SKILL.md` in wicked-garden.
pub const SECURITY_REVIEW_SKILL: &str = "wicked-garden-qe-security-test-engineer";

/// The twelve catalog ids, in the §8.3 table's order.
pub const CATALOG_IDS: [&str; 12] = [
    "understand",
    "test_plan",
    "design",
    "architecture",
    "build",
    "produce",
    "test",
    "review",
    "critique",
    "security_review",
    "run",
    "deliver",
];

/// The phase catalog: twelve entries, in [`CATALOG_IDS`] order. Built once; the slice is static.
pub fn catalog() -> &'static [PhaseDef] {
    static CATALOG: OnceLock<Vec<PhaseDef>> = OnceLock::new();
    // RED: the entries are not written yet.
    CATALOG.get_or_init(Vec::new)
}

/// One catalog entry by id (`None` for an id the catalog does not define).
pub fn catalog_entry(id: &str) -> Option<&'static PhaseDef> {
    catalog().iter().find(|e| e.id == id)
}

/// `true` for an entry whose executor is a Tool (`run`, `deliver`): the only entries a step may
/// hand an `executor`.
pub fn is_tool_entry(entry: &PhaseDef) -> bool {
    matches!(entry.executor, PhaseExecutor::Tool { .. })
}

fn build_catalog() -> Vec<PhaseDef> {
    use GateType::{Execution, Strategy, Value};
    use PhaseRole::{Creator, Evaluator, Neutral};
    use StageKind::{Build, Recon, Review, Test};
    let auto = GateSpec::Auto;
    let floor = || Some(EVIDENCE_FLOOR_PIN.to_string());
    let tool = || PhaseExecutor::Tool { cmd: Vec::new() };
    vec![
        entry("understand", Recon, Neutral, auto, Value, None, false),
        entry("test_plan", Test, Neutral, auto, Value, None, false),
        entry("design", Recon, Neutral, auto, Strategy, None, false),
        entry("architecture", Recon, Neutral, auto, Strategy, None, false),
        entry("build", Build, Creator, auto, Execution, floor(), true),
        entry("produce", Build, Creator, auto, Value, None, false),
        PhaseDef {
            // The one entry that declares re-verified evidence: its pin is what re-verifies it.
            verified_evidence: true,
            ..entry(
                "test",
                Test,
                Evaluator,
                GateSpec::HumanConfirmIf(GateCond::VerdictNotPass),
                Execution,
                floor(),
                false,
            )
        },
        entry("review", Review, Evaluator, auto, Execution, floor(), false),
        entry("critique", Review, Evaluator, auto, Execution, None, false),
        PhaseDef {
            skill_ref: Some(SECURITY_REVIEW_SKILL.to_string()),
            ..entry(
                "security_review",
                Review,
                Evaluator,
                auto,
                Execution,
                floor(),
                false,
            )
        },
        PhaseDef {
            executor: tool(),
            ..entry("run", Recon, Neutral, auto, Value, None, false)
        },
        PhaseDef {
            executor: tool(),
            ..entry("deliver", Build, Neutral, auto, Execution, None, false)
        },
    ]
}

fn entry(
    id: &str,
    kind: StageKind,
    role: PhaseRole,
    gate: GateSpec,
    gate_type: GateType,
    validator_pin: Option<String>,
    executes_code: bool,
) -> PhaseDef {
    PhaseDef {
        id: id.to_string(),
        kind,
        instructions: None,
        gate_type: Some(gate_type),
        gate,
        executes_code,
        verified_evidence: false,
        required_deliverables: Vec::new(),
        depends_on: Vec::new(),
        role,
        skill_ref: None,
        allowed_skills: Vec::new(),
        validator_pin,
        executor: PhaseExecutor::Agent,
        owner: StepOwner::Pa,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The §8.3 table, cell by cell, as fixed values (not re-derived from the builder).
    #[test]
    fn the_catalog_is_the_twelve_entries_of_the_table() {
        let got: Vec<_> = catalog()
            .iter()
            .map(|e| {
                (
                    e.id.as_str(),
                    serde_json::to_value(e.kind).unwrap(),
                    serde_json::to_value(e.role).unwrap(),
                    serde_json::to_value(e.gate).unwrap(),
                    serde_json::to_value(e.gate_type).unwrap(),
                    e.validator_pin.as_deref(),
                    e.executes_code,
                    serde_json::to_value(&e.executor).unwrap()["type"].clone(),
                    e.skill_ref.as_deref(),
                )
            })
            .collect();
        let j = |s: &str| serde_json::Value::String(s.to_string());
        let hci = serde_json::json!({"human_confirm_if": "verdict_not_pass"});
        let f = Some("e2e7af1db9e48454");
        #[rustfmt::skip]
        let want = vec![
            ("understand", j("recon"), j("neutral"), j("auto"), j("value"), None, false, j("agent"), None),
            ("test_plan", j("test"), j("neutral"), j("auto"), j("value"), None, false, j("agent"), None),
            ("design", j("recon"), j("neutral"), j("auto"), j("strategy"), None, false, j("agent"), None),
            ("architecture", j("recon"), j("neutral"), j("auto"), j("strategy"), None, false, j("agent"), None),
            ("build", j("build"), j("creator"), j("auto"), j("execution"), f, true, j("agent"), None),
            ("produce", j("build"), j("creator"), j("auto"), j("value"), None, false, j("agent"), None),
            ("test", j("test"), j("evaluator"), hci, j("execution"), f, false, j("agent"), None),
            ("review", j("review"), j("evaluator"), j("auto"), j("execution"), f, false, j("agent"), None),
            ("critique", j("review"), j("evaluator"), j("auto"), j("execution"), None, false, j("agent"), None),
            (
                "security_review",
                j("review"),
                j("evaluator"),
                j("auto"),
                j("execution"),
                f,
                false,
                j("agent"),
                Some("wicked-garden-qe-security-test-engineer"),
            ),
            ("run", j("recon"), j("neutral"), j("auto"), j("value"), None, false, j("tool"), None),
            ("deliver", j("build"), j("neutral"), j("auto"), j("execution"), None, false, j("tool"), None),
        ];
        assert_eq!(got, want);
        let ids: Vec<_> = catalog().iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, CATALOG_IDS);
    }

    /// The evidence floor moved onto the catalog (DES-TEAMING-002 §10): exactly the entries whose
    /// evidence is a code change carry it — the code-writing `build` and the code-judging `test`,
    /// `review` and `security_review` — and nothing that writes or judges prose does.
    #[test]
    fn the_evidence_floor_sits_on_the_code_entries_only() {
        let floored: Vec<_> = catalog()
            .iter()
            .filter(|e| e.validator_pin.as_deref() == Some(EVIDENCE_FLOOR_PIN))
            .map(|e| e.id.as_str())
            .collect();
        assert_eq!(floored, ["build", "test", "review", "security_review"]);
        assert!(catalog()
            .iter()
            .all(|e| e.validator_pin.is_none()
                || e.validator_pin.as_deref() == Some(EVIDENCE_FLOOR_PIN)));
    }
}
