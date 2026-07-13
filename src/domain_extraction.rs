//! DOMAIN-EXTRACTION — the authored artifacts that make the drop-in `workflows/domain-extraction.json`
//! GATED (DES-DOMAIN-BRAIN-CONTRACT §5, CONTRACT-3 §2/§4).
//!
//! The `domain-extraction` workflow is pure DATA (`workflows/domain-extraction.json`, loaded via
//! [`WorkflowRegistry::load_dir`](crate::workflow::WorkflowRegistry::load_dir) — zero core edit, Law 2).
//! Its `coverage` phase carries a [`validator_pin`](crate::workflow::PhaseDef::validator_pin) so the
//! rev0.4 dual-validator gate ENGAGES: at gate time crew re-runs an APPROVED deterministic validator in
//! the phase worktree with **no LLM**, and deny dominates (a `< 1.0` result rejects the phase before any
//! `work_output` is written).
//!
//! This module owns the one thing a data file cannot: the authored, port-of-`coverage.py --check`
//! deterministic validator, plus the **existing** provision → approve → vault path that mints its pin.
//! The pin is content-addressed ([`crate::validator_vault::pin`]), so it is deterministic and can be
//! embedded in the JSON; the [`tests`] re-derive it and assert the JSON, the builder, and the vaulted
//! approved copy all agree — a drifted script can never masquerade under the embedded pin.
//!
//! ## Disjoint-build boundary
//! crew GOVERNS. This module MOCKS garden/brain/estate exactly as the contract's disjoint rule requires:
//! the coverage validator asserts over a `coverage-report.json` *document* (brain's output shape,
//! `coverage.py:576-588`) — crew never imports brain, garden, or estate code, and never parses the
//! domain-model JSON's content. The only thing crossing the repo line is that document shape.

use crate::validator::DeterministicValidator;

/// The registered id of the drop-in workflow this module gates (`workflows/domain-extraction.json`).
pub const DOMAIN_EXTRACTION_WORKFLOW_ID: &str = "domain-extraction";

/// The acceptance criterion of the coverage gate — anti-legacy GATE_3 / `coverage.py` DoD
/// (CONTRACT-3 §2: "resolved-or-flagged coverage == 1.0 (zero unaccounted behavior-bearing nodes)").
pub const COVERAGE_CRITERION: &str =
    "resolved-or-flagged coverage == 1.0 (zero unaccounted behavior-bearing nodes)";

/// The deterministic re-verify (port of `coverage.py --check`): exit 0 IFF the phase worktree's
/// `coverage-report.json` reports FULL coverage EVERYWHERE. brain's report carries a top-level
/// `coverage`/`unaccounted` PLUS a per-app breakdown (each app object has its OWN `coverage`/`unaccounted`),
/// so an unanchored positive grep false-PASSes on a single fully-covered app under a sub-1.0 total. The
/// gate is therefore: (1) at least one full-coverage marker exists (guards an empty/malformed report),
/// AND (2) NO `coverage` value is sub-1.0 anywhere — every complete ratio starts with `1` (`1`/`1.0`/
/// `1.0000`), every incomplete one with `0` (`0`/`0.0`/`0.83`) — AND (3) NO `unaccounted` is non-zero
/// anywhere. Built only from `test`/`grep`/`!`/literal paths so it passes the
/// [`looks_dangerous`](crate::validator) denylist (no redirection, command substitution, or
/// destructive/network token; `!` negation is allowed).
pub const COVERAGE_SCRIPT: &str = r#"test -f coverage-report.json && grep -Eq '"coverage":[[:space:]]*(1|1\.0+)([,}[:space:]]|$)' coverage-report.json && ! grep -Eq '"coverage":[[:space:]]*0' coverage-report.json && ! grep -Eq '"unaccounted":[[:space:]]*[1-9]' coverage-report.json"#;

/// The APPROVED content-address pin the `coverage` phase carries in `workflows/domain-extraction.json`.
/// Content-hash over `(COVERAGE_CRITERION, COVERAGE_SCRIPT, approved=true)` — see
/// [`crate::validator_vault::pin`]. Re-derived and asserted equal to the vaulted approved copy and to
/// the JSON's embedded pin by [`tests::embedded_pin_matches_the_approved_vaulted_validator`]; if the
/// criterion or script ever changes, that test fails loudly and this const must be regenerated.
pub const COVERAGE_VALIDATOR_PIN: &str = "c4cc487a030d57b7";

/// The authored (UNAPPROVED) coverage validator — the artifact a human/council reviews before it can
/// gate. Authoring never authorizes running: `approved == false` (rev0.4 fork 3). Route it through the
/// vault ([`provision_and_approve_coverage_validator`]) to obtain the gate-ready approved pin.
#[must_use]
pub fn coverage_eq_one_validator() -> DeterministicValidator {
    DeterministicValidator {
        criterion: COVERAGE_CRITERION.to_string(),
        script: COVERAGE_SCRIPT.to_string(),
        approved: false,
    }
}

/// Author + approve + vault the coverage validator through the EXISTING provision/approve path, returning
/// the approved pin (== [`COVERAGE_VALIDATOR_PIN`]). This is the programmatic analogue of the operator
/// flow `wicked-core provision-validator` → `approve-validator`: it vaults the authored validator
/// UNAPPROVED, then performs the separate, audited approval step
/// ([`approve_and_store`](crate::validator_vault::approve_and_store)) — the approval that a human/council
/// owns. Unlike [`provision_validator`](crate::validator_vault::provision_validator) it does not run an
/// LLM writer skill, because this validator is a hand-authored port of `coverage.py --check`, not an
/// LLM-generated check. Runs on the actor (single-writer) thread via the vault's `put_node`.
pub fn provision_and_approve_coverage_validator(
    store: &mut wicked_apps_core::SqliteStore,
) -> anyhow::Result<String> {
    // 1. Vault the AUTHORED validator UNAPPROVED (authoring never authorizes running).
    let unapproved_pin =
        crate::validator_vault::store_validator(store, &coverage_eq_one_validator())?;
    // 2. The separate, audited APPROVAL step → the distinct approved pin a phase carries.
    let approved_pin = crate::validator_vault::approve_and_store(store, &unapproved_pin)?
        .ok_or_else(|| {
            anyhow::anyhow!("coverage validator vanished from the vault between store and approve")
        })?;
    Ok(approved_pin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::run_validator;
    use crate::validator_vault::{load_validator, pin};
    use crate::workflow::{GateCond, GateSpec, GateType, PhaseRole, WorkflowRegistry};
    use crate::{domain::StageKind, plan::plan_from_def};

    /// Load the shipped drop-in `workflows/domain-extraction.json` exactly as an operator's `load_dir`
    /// overlay would — parse + validate through the real registry path.
    fn load_shipped_def() -> crate::workflow::WorkflowDef {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("workflows")
            .join("domain-extraction.json");
        WorkflowRegistry::def_from_file(&path)
            .unwrap_or_else(|e| panic!("domain-extraction.json must parse + validate: {e}"))
    }

    #[test]
    fn shipped_workflow_loads_and_validates() {
        let def = load_shipped_def();
        assert_eq!(def.id, DOMAIN_EXTRACTION_WORKFLOW_ID);
        let ids: Vec<&str> = def.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["survey", "analyze", "extract", "coverage", "domain-graph"],
            "phases map anti-legacy's front half in order"
        );
        // The whole thing must satisfy the DAG/uniqueness invariants (backward-only depends_on).
        def.validate().expect("shipped def is a valid WorkflowDef");
    }

    #[test]
    fn load_dir_registers_the_drop_in_alongside_the_builtins() {
        // Law-2 proof: the real registry overlay path picks the file up with zero core edit, and the
        // built-ins survive alongside it.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        let mut reg = WorkflowRegistry::with_defaults();
        let loaded = reg.load_dir(&dir).expect("overlay loads");
        assert!(
            loaded.contains(&DOMAIN_EXTRACTION_WORKFLOW_ID.to_string()),
            "domain-extraction registered from data; loaded = {loaded:?}"
        );
        assert!(reg.get("feature").is_some(), "built-ins remain");
        assert!(reg.get(DOMAIN_EXTRACTION_WORKFLOW_ID).is_some());
    }

    #[test]
    fn coverage_phase_is_the_gated_evaluator_over_the_extract_creator() {
        // The evaluator≠creator attestation (CONTRACT-3 §2): extract CREATES the rule IP, coverage
        // EVALUATES it cold under a seat-distinct judge, gated on the approved coverage validator.
        let def = load_shipped_def();
        let creator = def
            .phases
            .iter()
            .find(|p| p.role == PhaseRole::Creator)
            .unwrap();
        let evaluator = def
            .phases
            .iter()
            .find(|p| p.role == PhaseRole::Evaluator)
            .unwrap();
        assert_eq!(creator.id, "extract");
        assert_eq!(evaluator.id, "coverage");
        // coverage depends_on extract (structural ordering — the DAG makes it non-negotiable).
        assert!(evaluator.depends_on.contains(&"extract".to_string()));

        let coverage = def.phases.iter().find(|p| p.id == "coverage").unwrap();
        assert_eq!(coverage.kind, StageKind::Test);
        assert_eq!(coverage.gate_type, Some(GateType::Execution));
        assert_eq!(
            coverage.gate,
            GateSpec::HumanConfirmIf(GateCond::VerdictNotPass)
        );
        assert!(
            coverage.verified_evidence,
            "coverage re-verifies evidence at the gate"
        );
        assert_eq!(
            coverage.validator_pin.as_deref(),
            Some(COVERAGE_VALIDATOR_PIN),
            "the coverage phase carries the approved coverage==1.0 pin"
        );
    }

    #[test]
    fn domain_graph_is_gated_human_confirm_after_coverage() {
        // CONTRACT-3 §2: the target requirements-graph is not built until coverage is a proven terminal;
        // its design gate is a human confirm.
        let def = load_shipped_def();
        let dg = def.phases.iter().find(|p| p.id == "domain-graph").unwrap();
        assert_eq!(dg.kind, StageKind::Build);
        assert_eq!(dg.gate_type, Some(GateType::Strategy));
        assert!(matches!(dg.gate, GateSpec::HumanConfirm { .. }));
        assert!(dg.depends_on.contains(&"coverage".to_string()));
    }

    #[test]
    fn every_phase_carries_a_garden_skill_ref_in_dash_form() {
        // CONTRACT-4 §3 SKILL NAMING: dash-form `wicked-<product>-<skill>`, never a colon namespace.
        let def = load_shipped_def();
        let expected = [
            ("survey", "wicked-garden-survey"),
            ("analyze", "wicked-garden-analyze"),
            ("extract", "wicked-garden-extract"),
            ("coverage", "wicked-garden-coverage-review"),
            ("domain-graph", "wicked-garden-domain-graph"),
        ];
        for (phase_id, skill) in expected {
            let phase = def.phases.iter().find(|p| p.id == phase_id).unwrap();
            assert_eq!(
                phase.skill_ref.as_deref(),
                Some(skill),
                "{phase_id} skill_ref"
            );
            assert!(
                !skill.contains(':'),
                "{skill} must be dash-form, not a colon namespace"
            );
            assert!(
                skill.starts_with("wicked-garden-"),
                "{skill} is a garden skill"
            );
        }
    }

    #[test]
    fn plan_from_def_carries_skill_refs_and_roles_onto_units() {
        // SkillRef wiring (deliverable 3): the phase skill_ref + least-privilege allowed_skills ride onto
        // the WorkUnit so the cli-runner invokes the right garden skill under the right brain-engine scope.
        let def = load_shipped_def();
        let units = plan_from_def(&def, "mine the legacy payments service", "s1");
        assert_eq!(units.len(), 5);
        assert_eq!(units[2].skill_ref.as_deref(), Some("wicked-garden-extract"));
        assert_eq!(
            units[2].allowed_skills,
            vec!["wicked-brain-domain".to_string()]
        );
        assert_eq!(
            units[3].skill_ref.as_deref(),
            Some("wicked-garden-coverage-review")
        );
        assert_eq!(
            units[3].allowed_skills,
            vec!["wicked-brain-coverage".to_string()]
        );
        // The evaluator≠creator role survives onto the units.
        assert_eq!(units[2].role, PhaseRole::Creator);
        assert_eq!(units[3].role, PhaseRole::Evaluator);
    }

    #[test]
    fn authored_validator_is_unapproved_and_matches_the_criterion() {
        let v = coverage_eq_one_validator();
        assert!(
            !v.approved,
            "authoring never authorizes running (fail-closed until approval)"
        );
        assert_eq!(v.criterion, COVERAGE_CRITERION);
        assert_eq!(v.script, COVERAGE_SCRIPT);
    }

    #[test]
    fn embedded_pin_matches_the_approved_vaulted_validator() {
        // The load-bearing tie: the pin embedded in workflows/domain-extraction.json == the pin of the
        // APPROVED coverage validator, minted through the real vault provision/approve path. A drifted
        // script would change the pin and fail here (tamper-evidence at author time).
        use wicked_apps_core::open_store;
        let dir =
            std::env::temp_dir().join(format!("wicked-domainext-vault-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut store = open_store(Some(dir.join("v.db").to_str().unwrap())).unwrap();

        let approved_pin = provision_and_approve_coverage_validator(&mut store).unwrap();
        assert_eq!(
            approved_pin, COVERAGE_VALIDATOR_PIN,
            "approved pin drifted from the const embedded in the JSON — regenerate COVERAGE_VALIDATOR_PIN"
        );

        // And it resolves back out of the vault as an APPROVED validator (gate-ready).
        let loaded = load_validator(&store, &approved_pin)
            .unwrap()
            .expect("present");
        assert!(loaded.approved, "the vaulted copy is approved");
        assert_eq!(pin(&loaded), COVERAGE_VALIDATOR_PIN);

        // And the JSON's coverage phase carries exactly this pin.
        let def = load_shipped_def();
        let coverage = def.phases.iter().find(|p| p.id == "coverage").unwrap();
        assert_eq!(
            coverage.validator_pin.as_deref(),
            Some(approved_pin.as_str())
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn approved_validator_passes_on_full_coverage_and_fails_otherwise() {
        // The deterministic re-verify behaves like `coverage.py --check`: exit 0 iff coverage == 1.0 and
        // there are zero unaccounted behavior-bearing nodes. We MOCK brain's coverage-report.json output.
        use wicked_apps_core::open_store;
        let base =
            std::env::temp_dir().join(format!("wicked-domainext-cov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mut store = open_store(Some(base.join("v.db").to_str().unwrap())).unwrap();
        let approved_pin = provision_and_approve_coverage_validator(&mut store).unwrap();
        let approved = load_validator(&store, &approved_pin).unwrap().unwrap();

        // PASS worktree: a coverage-report.json at full coverage with zero unaccounted nodes.
        let pass_wt = base.join("pass");
        std::fs::create_dir_all(&pass_wt).unwrap();
        std::fs::write(
            pass_wt.join("coverage-report.json"),
            r#"{
  "total": 42,
  "behavior_bearing": 30,
  "resolved": 28,
  "risk_flagged": 2,
  "unaccounted": 0,
  "coverage": 1.0,
  "resolve_threshold": 0.75,
  "unaccounted_nodes": []
}"#,
        )
        .unwrap();
        assert!(
            run_validator(&approved, &pass_wt).unwrap(),
            "coverage == 1.0 with zero unaccounted ⇒ gate PASSES"
        );

        // FAIL worktree: a coverage hole (coverage < 1.0, unaccounted > 0).
        let fail_wt = base.join("fail");
        std::fs::create_dir_all(&fail_wt).unwrap();
        std::fs::write(
            fail_wt.join("coverage-report.json"),
            r#"{
  "total": 42,
  "behavior_bearing": 30,
  "resolved": 25,
  "risk_flagged": 0,
  "unaccounted": 5,
  "coverage": 0.8333,
  "resolve_threshold": 0.75,
  "unaccounted_nodes": ["sym::a", "sym::b", "sym::c", "sym::d", "sym::e"]
}"#,
        )
        .unwrap();
        assert!(
            !run_validator(&approved, &fail_wt).unwrap(),
            "coverage < 1.0 (unaccounted > 0) ⇒ gate FAILS (deny dominates)"
        );

        // MISSING evidence: no coverage-report.json at all ⇒ fail-closed, never a silent pass.
        let empty_wt = base.join("empty");
        std::fs::create_dir_all(&empty_wt).unwrap();
        assert!(
            !run_validator(&approved, &empty_wt).unwrap(),
            "absent coverage evidence ⇒ gate FAILS closed"
        );

        // REGRESSION — the per-app false-pass. brain's coverage-report.json carries a per_app
        // breakdown, each app object with its OWN coverage/unaccounted. A single fully-covered app
        // under a sub-1.0 TOTAL must NOT satisfy the gate; the old unanchored positive greps matched
        // the per_app "coverage":1.0 / "unaccounted":0 lines and false-PASSed. deny-dominates → FAIL.
        let per_app_wt = base.join("per_app");
        std::fs::create_dir_all(&per_app_wt).unwrap();
        std::fs::write(
            per_app_wt.join("coverage-report.json"),
            r#"{
  "total": 42,
  "behavior_bearing": 30,
  "resolved": 25,
  "risk_flagged": 0,
  "unaccounted": 5,
  "coverage": 0.8333,
  "per_app": {
    "billing": { "coverage": 1.0, "unaccounted": 0 },
    "shipping": { "coverage": 0.6, "unaccounted": 5 }
  }
}"#,
        )
        .unwrap();
        assert!(
            !run_validator(&approved, &per_app_wt).unwrap(),
            "sub-1.0 TOTAL with a fully-covered per_app entry ⇒ gate FAILS (no per-app false-pass)"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn unapproved_coverage_validator_refuses_to_run() {
        // Fail-closed: the authored-but-unapproved validator cannot gate (rev0.4 fork 3).
        let base =
            std::env::temp_dir().join(format!("wicked-domainext-unappr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("coverage-report.json"),
            r#"{"unaccounted": 0, "coverage": 1.0}"#,
        )
        .unwrap();
        let unapproved = coverage_eq_one_validator();
        assert!(
            run_validator(&unapproved, &base).is_err(),
            "an UNAPPROVED validator must refuse to run even where the criterion is satisfied"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
