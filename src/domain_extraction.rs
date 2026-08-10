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
    "at least one behavior-bearing node, and resolved-or-flagged coverage == 1.0 over them (zero unaccounted)";

/// The deterministic re-verify (port of `coverage.py --check`): exit 0 IFF FULL coverage EVERYWHERE.
/// When `WICKED_COVERAGE_DB` is set (injected by the validator runner from the actor's store path — its
/// own carrier, so the OPERATIONAL `WICKED_ESTATE_DB` can be removed from the validator env entirely,
/// core#166), the gate ALWAYS RECOMPUTES `coverage-report.json` from that store via `wicked-core
/// coverage`, OVERWRITING any report the creator worker produced, then checks it. This is deliberate
/// evaluator≠creator: the gate must not trust the worker's SELF-produced coverage-report.json. Doing so
/// was the #9b bug — the domain-coverage skill runs `wicked-core coverage --db "${WICKED_ESTATE_DB:-…}"`
/// but the governed worker's shell has no `WICKED_ESTATE_DB` (stripped by `hardened()`, FINDING-067), so
/// it fell back to a STALE global `~/.wicked-estate/graph.db` (0 behavior-bearing) and the old
/// `test -f coverage-report.json ||` short-circuit accepted that stale file, denying a repo whose real
/// store has 766 behavior-bearing nodes. Recomputing from `WICKED_COVERAGE_DB` (the repo's own
/// `code_graph_db`, per FINDING-091/#009) is authoritative. Without `WICKED_COVERAGE_DB` (e.g. standalone
/// tests), it FALLS BACK to a pre-existing file and FAILS CLOSED if absent. Crucially, the file
/// fallback is guarded by `test -z "${WICKED_COVERAGE_DB}"` so it applies ONLY when NO store was given:
/// when the store IS set, a recompute that EXITS NON-ZERO FAILS CLOSED rather than silently trusting a
/// pre-existing (stale/creator) report — a recompute failure must not degrade to the trust it replaced
/// (Copilot review on #230). Shape (sh equal-precedence left-assoc `&&`/`||`, subshells binding each
/// branch): `( test -n DB && recompute ) || ( test -z DB && test -f file ) && test -f file && <greps>`
/// = `(recompute-if-store ELSE file-only-if-no-store) && validate`. brain's report
/// carries a top-level `coverage`/`unaccounted` PLUS a per-app breakdown (each app object has its OWN
/// `coverage`/`unaccounted`), so an unanchored positive grep false-PASSes on a single fully-covered app
/// under a sub-1.0 total. The gate is therefore: (1) at least one full-coverage marker exists (guards an
/// empty/malformed report), AND (2) NO `coverage` value is sub-1.0 anywhere, AND (3) NO `unaccounted`
/// is non-zero anywhere. Built only from `test`/`grep`/`!`/`${VAR:-fallback}` so it passes the
/// [`looks_dangerous`](crate::validator) denylist (no redirection, command substitution `$(`, or
/// destructive/network token; `${VAR}`/`${VAR:-default}` expansion and `!`/`||` are allowed).
/// The binary is invoked as `${WICKED_CORE_EXE:-wicked-core}`. The validator runner resolves and
/// injects that path (`execute_wrapped::resolve_wicked_core_exe_opt`) so the script finds the right
/// binary without relying on PATH. It used to inject `current_exe()`, which is the NODE binary when
/// the engine runs as a napi addon — so the script ran `node coverage` and the floor never executed
/// on a real run (FINDING-093).
pub const COVERAGE_SCRIPT: &str = r#"( test -n "${WICKED_COVERAGE_DB}" && "${WICKED_CORE_EXE:-wicked-core}" coverage ) || ( test -z "${WICKED_COVERAGE_DB}" && test -f coverage-report.json ) && test -f coverage-report.json && grep -Eq '"coverage":[[:space:]]*(1|1\.0+)([,}[:space:]]|$)' coverage-report.json && ! grep -Eq '"coverage":[[:space:]]*0' coverage-report.json && ! grep -Eq '"unaccounted":[[:space:]]*[1-9]' coverage-report.json && grep -Eq '"behavior_bearing":[[:space:]]*[1-9]' coverage-report.json"#;

/// The coverage report's filename, relative to the run's worktree.
///
/// Named once because three things have to agree on it AT RUNTIME: [`COVERAGE_SCRIPT`] (which reads
/// it and, on the fallback branch, causes it to be written), `wicked-core coverage` (which writes it
/// into its cwd), and the denial diagnostic that reads it back to report what was measured.
///
/// `workflows/domain-extraction.json` also lists the file in the `coverage` phase's
/// `required_deliverables`, but that field is deserialized into
/// [`PhaseDef`](crate::workflow::PhaseDef) and never read by anything — it documents an intent the
/// engine does not enforce, so it is NOT a fourth agreeing artifact (review; filed as FINDING-101).
///
/// The fourth disagreed. `failing_measurement` looked in `.wicked/domain/coverage-report.json` — a
/// path nothing else in the system uses — so it never found the report, and every LEGITIMATE
/// coverage denial was reported as "no coverage report was produced, so the criterion could not be
/// measured" (FINDING-099). That is a false cause: the floor had measured, and the number was
/// simply bad. It is precisely the mis-attribution FINDING-092 was written to end, and it made the
/// denial indistinguishable from FINDING-093's genuinely-inert floor.
///
/// This constant does not appear inside [`COVERAGE_SCRIPT`] by interpolation — that string is
/// content-addressed by [`COVERAGE_VALIDATOR_PIN`] and changing its bytes would cascade across the
/// pin, the shipped drop-in, crew's mirror and crew's test. The guard in this module's tests
/// asserts the script CONTAINS this name instead, which is the same P1 rule applied without
/// disturbing the hash.
pub const COVERAGE_REPORT_FILE: &str = "coverage-report.json";

/// The APPROVED content-address pin the `coverage` phase carries in `workflows/domain-extraction.json`.
/// Content-hash over `(COVERAGE_CRITERION, COVERAGE_SCRIPT, approved=true)` — see
/// [`crate::validator_vault::pin`]. Re-derived and asserted equal to the vaulted approved copy and to
/// the JSON's embedded pin by [`tests::embedded_pin_matches_the_approved_vaulted_validator`]; if the
/// criterion or script ever changes, that test fails loudly and this const must be regenerated.
pub const COVERAGE_VALIDATOR_PIN: &str = "49b61ba3ab5264e4";

/// Phases whose `validator_pin` the BINARY has an opinion about, as `(workflow, phase, pin)`.
///
/// The engine dispatches the def installed in the user's config dir, NOT the one in this repo. Those
/// two drift the moment a pin changes and an install is not refreshed — observed live: the installed
/// `domain-extraction.json` still pinned `4a4b10bf4277bd34` while this binary had moved to
/// `e7f84b91d030fdcc`, so a run would have gated on the PRE-substance-rule validator and reported
/// success (FINDING-080).
///
/// `lockstep.rs` already asserts this constant matches the REPO's JSON. Nothing asserted it against
/// the INSTALLED JSON, which is the only copy that ever executes — one artifact further out than any
/// existing guard reached (wicked-core#186).
pub const BINARY_PINNED_PHASES: &[(&str, &str, &str)] =
    &[("domain-extraction", "coverage", COVERAGE_VALIDATOR_PIN)];

/// An installed def whose pinned phase disagrees with this binary.
#[derive(Debug, Clone, PartialEq)]
pub struct PinMismatch {
    pub workflow: String,
    pub phase: String,
    /// What the INSTALLED def carries — `None` when the phase lost its pin entirely, which is worse:
    /// the phase would run ungated.
    pub installed: Option<String>,
    /// What this binary expects.
    pub expected: &'static str,
}

impl std::fmt::Display for PinMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "installed workflow `{}` phase `{}` pins {} but this engine expects {} — the installed \
             def is stale. Re-install the drop-in defs, and seed/approve the validator FIRST \
             (`wicked-core seed-domain-validators --db <store>`) or every run will fail closed on an \
             unresolvable pin",
            self.workflow,
            self.phase,
            self.installed.as_deref().unwrap_or("NOTHING (phase is ungated)"),
            self.expected
        )
    }
}

/// Compare every installed def against [`BINARY_PINNED_PHASES`].
///
/// Returns the disagreements rather than logging them, so the caller decides the policy: the actor
/// removes the def (a stale gate must not dispatch), while a test can assert on the list.
#[must_use]
pub fn installed_pin_mismatches(reg: &crate::workflow::WorkflowRegistry) -> Vec<PinMismatch> {
    let mut out = Vec::new();
    for (wf, phase_id, expected) in BINARY_PINNED_PHASES {
        let Some(def) = reg.get(wf) else { continue };
        let Some(phase) = def.phases.iter().find(|p| p.id == *phase_id) else {
            continue;
        };
        if phase.validator_pin.as_deref() != Some(*expected) {
            out.push(PinMismatch {
                workflow: (*wf).to_string(),
                phase: (*phase_id).to_string(),
                installed: phase.validator_pin.clone(),
                expected,
            });
        }
    }
    out
}

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
    store: &mut dyn wicked_apps_core::GraphStore,
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
    /// FINDING-080 / wicked-core#186: the def that DISPATCHES is the installed one, and nothing
    /// compared it to the binary. Observed live — the installed `domain-extraction.json` still
    /// pinned `4a4b10bf4277bd34` while the binary had moved to `e7f84b91d030fdcc`, so a run would
    /// have gated on the pre-substance-rule validator and reported success.
    #[test]
    fn an_installed_def_pinning_a_validator_this_binary_does_not_know_is_reported() {
        // `domain-extraction` ships as a DROP-IN, not a compiled built-in (FINDING-074), so it must
        // be loaded the way the engine loads it — from the workflows dir.
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        let mut reg = crate::workflow::WorkflowRegistry::with_defaults();
        reg.load_dir(&dir).expect("overlay loads");
        // The SHIPPED def must already agree with the binary — otherwise the guard would fire on a
        // clean tree and this test could not distinguish stale from normal.
        assert!(
            installed_pin_mismatches(&reg).is_empty(),
            "the SHIPPED drop-in already disagrees with this binary's pins"
        );

        let mut def = reg
            .get("domain-extraction")
            .expect("domain-extraction is registered")
            .clone();
        let phase = def
            .phases
            .iter_mut()
            .find(|p| p.id == "coverage")
            .expect("coverage phase");
        phase.validator_pin = Some("4a4b10bf4277bd34".to_string()); // the real stale pin
        reg.register(def).expect("replace with the stale def");

        let found = installed_pin_mismatches(&reg);
        assert_eq!(found.len(), 1, "expected exactly one mismatch: {found:?}");
        assert_eq!(found[0].installed.as_deref(), Some("4a4b10bf4277bd34"));
        assert_eq!(found[0].expected, COVERAGE_VALIDATOR_PIN);
        // The message has to be actionable: both values AND the seed-first ordering, because
        // refreshing the def before seeding the validator fails every run closed.
        let msg = found[0].to_string();
        assert!(
            msg.contains("4a4b10bf4277bd34") && msg.contains(COVERAGE_VALIDATOR_PIN),
            "{msg}"
        );
        assert!(
            msg.contains("seed-domain-validators"),
            "no remedy named: {msg}"
        );
    }

    /// Review of the first version of this fix caught that `remove()` cannot "fall back to the
    /// compiled built-in": `register` overwrites by id, so nothing is left behind — and
    /// `domain-extraction` has no compiled form at all, being a drop-in. Removal traded a wrong gate
    /// for an unknown-workflow failure.
    ///
    /// So the repair must leave the workflow AVAILABLE and correctly pinned. Both halves are
    /// asserted, because fixing the pin while losing the workflow is not a fix.
    #[test]
    fn repairing_a_stale_pin_corrects_it_and_keeps_the_workflow_dispatchable() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        let mut reg = crate::workflow::WorkflowRegistry::with_defaults();
        reg.load_dir(&dir).expect("overlay loads");

        let mut def = reg.get("domain-extraction").expect("registered").clone();
        def.phases
            .iter_mut()
            .find(|p| p.id == "coverage")
            .expect("coverage phase")
            .validator_pin = Some("4a4b10bf4277bd34".to_string());
        reg.register(def).expect("install the stale def");
        assert_eq!(
            installed_pin_mismatches(&reg).len(),
            1,
            "stale def should be flagged"
        );

        for m in installed_pin_mismatches(&reg) {
            assert!(
                reg.repin(&m.workflow, &m.phase, m.expected),
                "repin should succeed"
            );
        }

        assert!(
            installed_pin_mismatches(&reg).is_empty(),
            "the pin was not corrected"
        );
        // The half `remove()` got wrong: the workflow must still be there to dispatch.
        let after = reg.get("domain-extraction").expect(
            "the workflow must remain registered — removing it trades a wrong gate for an unknown one",
        );
        assert_eq!(
            after
                .phases
                .iter()
                .find(|p| p.id == "coverage")
                .and_then(|p| p.validator_pin.as_deref()),
            Some(COVERAGE_VALIDATOR_PIN)
        );
    }

    /// The sibling case — a replacement that DROPS the pin — turns out to be unreachable through
    /// the registry: `carry_shadowed_pins` carries a shadowed pin forward and announces the
    /// substitution, precisely so a hand-copied def cannot take a gate back out silently.
    ///
    /// So this asserts that EXISTING protection rather than the mismatch reporter. `PinMismatch`
    /// still models `installed: None` defensively, but nothing in the registry can produce it, and
    /// a test asserting otherwise would be asserting an impossible state.
    #[test]
    fn a_replacement_that_drops_the_pin_has_it_carried_forward_not_reported_as_stale() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("workflows");
        let mut reg = crate::workflow::WorkflowRegistry::with_defaults();
        reg.load_dir(&dir).expect("overlay loads");

        let mut def = reg.get("domain-extraction").expect("registered").clone();
        def.phases
            .iter_mut()
            .find(|p| p.id == "coverage")
            .expect("coverage phase")
            .validator_pin = None;
        reg.register(def).expect("replace");

        let after = reg
            .get("domain-extraction")
            .and_then(|d| d.phases.iter().find(|p| p.id == "coverage"))
            .and_then(|p| p.validator_pin.clone());
        assert_eq!(
            after.as_deref(),
            Some(COVERAGE_VALIDATOR_PIN),
            "a replacement dropping the pin must have it carried forward, not lost"
        );
        assert!(
            installed_pin_mismatches(&reg).is_empty(),
            "the carried-forward pin still matches the binary, so nothing is stale"
        );
    }

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
        // PERSIST FIX (core#237): domain-graph is a DETERMINISTIC Tool that runs `wicked-core
        // domain-graph --db {code_graph_db}`, which builds AND PERSISTS the domain/requirement/rule
        // graph into the repo store — not an LLM skill that could hit a non-persisting hermetic
        // fallback (which left the store empty and the milestone unverifiable). Mutation: revert it
        // to the `wicked-garden-domain-modeler` skill phase (executor → Agent) and this fails.
        match &dg.executor {
            crate::workflow::PhaseExecutor::Tool { cmd } => {
                assert_eq!(cmd.first().map(String::as_str), Some("wicked-core"));
                assert!(
                    cmd.iter().any(|a| a == "domain-graph"),
                    "runs the domain-graph subcommand"
                );
                assert!(
                    cmd.iter().any(|a| a == crate::workflow::CODE_GRAPH_DB_TOKEN),
                    "targets the repo's own code graph via {} so the persist lands in the repo store",
                    crate::workflow::CODE_GRAPH_DB_TOKEN
                );
            }
            other => panic!(
                "domain-graph must persist via a `wicked-core domain-graph` Tool executor, not {other:?}"
            ),
        }
        assert!(
            dg.skill_ref.is_none(),
            "domain-graph is a deterministic Tool, not a skill (no LLM hermetic-fallback lane)"
        );
    }

    #[test]
    fn skill_phases_carry_a_garden_skill_ref_in_dash_form() {
        // CONTRACT-4 §3 SKILL NAMING: dash-form `wicked-<product>-<skill>`, never a colon namespace.
        // The four AGENTIC front phases are garden skills; `domain-graph` is NOT — it is a
        // deterministic `wicked-core domain-graph` Tool executor (core#237 persist fix) and carries
        // no skill_ref (asserted in `domain_graph_is_gated_human_confirm_after_coverage`).
        let def = load_shipped_def();
        // Retargeted to the REAL garden `domain` surface (core#43) — renamed from `modernize`.
        let expected = [
            ("survey", "wicked-garden-domain"),
            ("analyze", "wicked-garden-domain"),
            ("extract", "wicked-garden-domain-extractor"),
            ("coverage", "wicked-garden-domain-coverage"),
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
        // The `wicked-brain-*` allowed_skills are RETIRED — every phase's allowlist is now empty, so a
        // leftover on ANY phase (not just the two pinned below) fails CI (reviewed correction).
        for phase in &def.phases {
            assert!(
                phase.allowed_skills.is_empty(),
                "phase {} still carries a dead allowed_skills entry: {:?}",
                phase.id,
                phase.allowed_skills
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
        assert_eq!(
            units[2].skill_ref.as_deref(),
            Some("wicked-garden-domain-extractor")
        );
        assert!(
            units[2].allowed_skills.is_empty(),
            "the retired brain allowlist is gone"
        );
        assert_eq!(
            units[3].skill_ref.as_deref(),
            Some("wicked-garden-domain-coverage")
        );
        assert!(units[3].allowed_skills.is_empty());
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

    /// The criterion is what an operator READS when a gate denies; the script is what actually
    /// decides. They are two descriptions of one rule, and they drifted the moment the script gained
    /// a requirement the prose did not mention — a vacuous report would deny while the stated
    /// criterion looked satisfied (`coverage: 1.0`, `unaccounted: 0`). Caught in review.
    ///
    /// That is FINDING-050's shape: a denial whose stated reason does not match its cause. Both
    /// strings are inputs to the content-address, so they cannot drift silently in the pin — but
    /// they can drift in MEANING, which is what this asserts.
    #[test]
    fn the_criterion_describes_the_requirement_the_script_enforces() {
        let script_checks_non_vacuity =
            COVERAGE_SCRIPT.contains(r#""behavior_bearing":[[:space:]]*[1-9]"#);
        let criterion_says_so = COVERAGE_CRITERION.contains("at least one behavior-bearing");
        assert_eq!(
            script_checks_non_vacuity, criterion_says_so,
            "the predicate and the criterion disagree about non-vacuity.\n  script enforces it: \
             {script_checks_non_vacuity}\n  criterion states it: {criterion_says_so}\nAn operator \
             reads the criterion to understand a denial; if it omits a requirement the script \
             enforces, the denial is unexplainable (FINDING-050)."
        );
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

    /// FINDING-009: "fully covered" and "nothing measured" must not be the same answer.
    ///
    /// `coverage` is `(resolved + risk_flagged) / behavior_bearing`, defined as 1.0 when the
    /// denominator is zero. Fine as arithmetic. As a GATE it meant a repo where nothing was
    /// annotated satisfied "zero unaccounted behavior-bearing nodes" trivially — there were none to
    /// account for.
    ///
    /// Verified against the real thing before this fix: `GET /governance/coverage` returned
    /// `{"total":691,"behavior_bearing":0,"coverage":1,"unaccounted":0}` — 691 being the daemon's
    /// own sessions and units, not any repo's code — and that payload PASSED the shipped predicate.
    ///
    /// This is FINDING-131's accounting-without-substance one level up: not in the annotations, but
    /// in the gate that checks them.
    #[test]
    fn a_report_over_zero_behavior_bearing_nodes_does_not_pass() {
        let dir = std::env::temp_dir().join(format!("cov_vacuous_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // The exact shape observed live.
        let vacuous = r#"{"total":691,"behavior_bearing":0,"resolved":0,"risk_flagged":0,"unaccounted":0,"coverage":1}"#;
        std::fs::write(dir.join("coverage-report.json"), vacuous).unwrap();
        assert!(
            !run_predicate(&dir),
            "a report over ZERO behavior-bearing nodes passed the gate — 100% of nothing is not \
             coverage (FINDING-009)"
        );

        // And the property this must not break: a genuinely complete report still passes.
        let real = r#"{"total":900,"behavior_bearing":42,"resolved":42,"risk_flagged":0,"unaccounted":0,"coverage":1}"#;
        std::fs::write(dir.join("coverage-report.json"), real).unwrap();
        assert!(
            run_predicate(&dir),
            "a real, fully-accounted report must still pass, or the gate denies every honest run"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #9b FIX: when `WICKED_COVERAGE_DB` is set, the gate RECOMPUTES from that store (authoritative,
    /// evaluator≠creator) and must NOT trust a stale creator-produced `coverage-report.json`. The bug:
    /// the governed worker's domain-coverage skill wrote a report from a STALE global store (0
    /// behavior-bearing, because its `WICKED_ESTATE_DB` was unset), and the old `test -f
    /// coverage-report.json ||` FIRST-ordering accepted that file, denying a repo whose real store has
    /// behavior-bearing nodes. Here: plant a stale 0-bb report; point `WICKED_CORE_EXE` at a stub that
    /// writes a GOOD report (standing in for `wicked-core coverage` reading the real repo store);
    /// `WICKED_COVERAGE_DB` non-empty ⇒ the gate must recompute (overwrite the stale file) and PASS.
    /// Mutation: restore `(test -f coverage-report.json || (test -n DB && …))` and this fails — the
    /// stale 0-bb file is trusted, greps fail on `behavior_bearing:0`, gate denies.
    #[test]
    fn coverage_gate_recomputes_from_the_store_and_ignores_a_stale_creator_report() {
        use wicked_apps_core::HardenedCommand;
        let dir = std::env::temp_dir().join(format!("cov_recompute_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Stale creator report: vacuous 0-behavior-bearing → would FAIL the greps if trusted.
        std::fs::write(
            dir.join("coverage-report.json"),
            r#"{"total":2188,"behavior_bearing":0,"resolved":0,"risk_flagged":0,"unaccounted":0,"coverage":1}"#,
        )
        .unwrap();

        // Stub standing in for `wicked-core coverage` reading the REAL repo store: writes a GOOD report
        // into cwd, ignoring its arg. (The script invokes `"${WICKED_CORE_EXE}" coverage`.)
        let stub = dir.join("stub-wicked-core.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\ncat > coverage-report.json <<'JSON'\n{\"total\":900,\"behavior_bearing\":42,\"resolved\":42,\"risk_flagged\":0,\"unaccounted\":0,\"coverage\":1.0}\nJSON\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let ok = std::process::Command::new("sh")
            .hardened()
            .arg("-c")
            .arg(COVERAGE_SCRIPT)
            .current_dir(&dir)
            // Set AFTER hardened() (which clears the env), exactly as the validator runner injects them.
            .env(crate::gate_hook::COVERAGE_DB_ENV, dir.join("repo.db")) // non-empty ⇒ recompute branch
            .env(crate::gate_hook::WICKED_CORE_EXE_ENV, &stub)
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        assert!(
            ok,
            "gate must RECOMPUTE from the store (stub) and pass — not trust the stale 0-bb creator report"
        );

        // The recompute overwrote the stale file with the authoritative one.
        let after = std::fs::read_to_string(dir.join("coverage-report.json")).unwrap();
        assert!(
            after.contains("\"behavior_bearing\":42"),
            "recompute must overwrite the stale creator report: {after}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FAIL-CLOSED on recompute failure (Copilot review on #230): when `WICKED_COVERAGE_DB` IS set but
    /// the recompute EXITS NON-ZERO, the gate must NOT fall back to trusting a pre-existing (stale,
    /// otherwise-passing) `coverage-report.json` — a recompute failure must fail closed, not degrade to
    /// the very trust this fix removes. The file fallback is guarded by `test -z "${WICKED_COVERAGE_DB}"`
    /// so it applies ONLY when no store was provided. Mutation: drop that guard (revert to
    /// `… coverage || test -f coverage-report.json`) and this fails — the stale passing report is
    /// accepted despite the recompute failing.
    #[test]
    fn coverage_gate_fails_closed_when_recompute_fails_even_if_a_passing_report_exists() {
        use wicked_apps_core::HardenedCommand;
        let dir = std::env::temp_dir().join(format!("cov_failclosed_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // A pre-existing report that WOULD pass the greps if trusted.
        std::fs::write(
            dir.join("coverage-report.json"),
            r#"{"total":900,"behavior_bearing":42,"resolved":42,"risk_flagged":0,"unaccounted":0,"coverage":1.0}"#,
        )
        .unwrap();

        // Stub standing in for a FAILED recompute: exits non-zero, writes nothing.
        let stub = dir.join("stub-fail.sh");
        std::fs::write(&stub, "#!/bin/sh\nexit 3\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let ok = std::process::Command::new("sh")
            .hardened()
            .arg("-c")
            .arg(COVERAGE_SCRIPT)
            .current_dir(&dir)
            .env(crate::gate_hook::COVERAGE_DB_ENV, dir.join("repo.db")) // store IS set
            .env(crate::gate_hook::WICKED_CORE_EXE_ENV, &stub) // …but recompute fails
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        assert!(
            !ok,
            "a recompute failure with the store set must FAIL CLOSED, not trust the stale passing report"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Run the SHIPPED predicate, not a paraphrase of it — a re-typed copy would drift from the
    /// string the validator is content-addressed over, and then this test would guard a fiction.
    fn run_predicate(dir: &std::path::Path) -> bool {
        use wicked_apps_core::HardenedCommand;
        std::process::Command::new("sh")
            // The rule has no exceptions, not even here: `spawn.rs` argues that an allowlist is a
            // standing invitation, and a test spawning a shell is exactly where someone would argue
            // for one. The audit caught this site the moment it appeared, which is the point.
            .hardened()
            .arg("-c")
            .arg(COVERAGE_SCRIPT)
            .current_dir(dir)
            .status()
            .map(|st| st.success())
            .unwrap_or(false)
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
