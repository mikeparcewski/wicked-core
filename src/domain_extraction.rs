//! DOMAIN-EXTRACTION — the authored artifacts that make the `domain-extraction` built-in preset
//! GATED (DES-DOMAIN-BRAIN-CONTRACT §5, CONTRACT-3 §2/§4).
//!
//! The `domain-extraction` workflow is DATA: the built-in preset (`crate::catalog`, X-MIG M6; the
//! retired `workflows/domain-extraction.json` drop-in is gone since M11). Its `coverage` step carries a [`validator_pin`](crate::workflow::PhaseDef::validator_pin) so the
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

/// The id of the built-in preset this module gates (`domain-extraction`).
pub const DOMAIN_EXTRACTION_WORKFLOW_ID: &str = "domain-extraction";

/// The acceptance criterion of the coverage gate — anti-legacy GATE_3 / `coverage.py` DoD
/// (CONTRACT-3 §2: "resolved-or-flagged coverage == 1.0 (zero unaccounted behavior-bearing nodes)").
/// Substance check added: at least one requirement must be genuinely resolved (not solely RISK-flagged),
/// closing the gameable-by-RISK-placeholders case (FINDING-D1, 2026-08-12).
///
/// The bar is the CONFIGURED MINIMUM (operator ruling 2026-10-31: 80% by default, crew's
/// `requirementsGraph.minCoveragePct`), not 1.0: a large repository is mapped hub-first over several
/// runs, and the graph is built and shown, labelled with its share, once the minimum is reached.
/// Without a store to recompute from, the file-only fallback demands full coverage of REAL
/// requirements: zero unaccounted and zero placeholders (codex on #1234).
///
/// REAL requirements only (operator decision 2026-11-01, core#1233): the minimum counts resolved
/// behaviours in product code; risk-flagged placeholders are shown in the graph as unconfirmed but
/// never count, and tests, data, manifests and documents are not in the denominator. The script is
/// unchanged: `wicked-core coverage --min-coverage-pct` decides.
///
/// NO NUMBER (e2e F29, run 1ec395da): this text is also the pinned judge's criterion
/// (`validator::pinned_judge_criterion`). It used to name "default 80%" and a no-store branch the
/// judge cannot see, so after the person lowered the repository's minimum to 66% the judge re-derived
/// the share from a stale report in the work against the 80% written here and rejected twice while
/// the engine's own recompute passed. The minimum is whatever the engine applies from the run's
/// store; the judge is told that number beside this text ([`coverage_judge_note`]).
pub const COVERAGE_CRITERION: &str =
    "at least one behavior-bearing node in product code; real requirements (resolved behaviours; risk-flagged placeholders never count, and placeholders may remain) at or above the repository's minimum, which the engine recomputes from the run's store: the repository's own target when one is set (the person may lower it during the run), else the daemon's setting. The engine's recompute decides the share and the minimum; a share, minimum or met/not-met flag written in the work does not. Only when the engine has no store to recompute from must every behaviour be a real requirement, and the engine decides which applies; and at least one requirement genuinely resolved (resolved > 0)";

/// The keys a domain-extraction scope may carry (garden `scripts/domain/run.py` `SCOPE_KEYS`, which
/// re-checks the same shape when the step reads it).
pub const SCOPE_KEYS: [&str; 4] = ["intent", "phrases", "scope_hash", "description"];

/// The largest scope a launch may carry, as compact JSON (garden's `SCOPE_MAX_BYTES`).
pub const SCOPE_MAX_BYTES: usize = 64 * 1024;

/// (J6 C1b) The launch's check of [`crate::LaunchSpec::scope`] against the workflow it names: a
/// `domain-extraction` launch needs a scope, and no other launch takes one. A scope is a JSON object
/// of [`SCOPE_KEYS`] only, with `intent` a non-empty string, `phrases` (when present, `null`
/// included) a list of non-empty strings, `scope_hash` and `description` strings, and at most
/// [`SCOPE_MAX_BYTES`]. Every refusal names what is wrong.
pub fn check_launch_scope(
    workflow: Option<&str>,
    scope: Option<&serde_json::Value>,
) -> Result<(), String> {
    let domain = workflow == Some(DOMAIN_EXTRACTION_WORKFLOW_ID);
    let Some(scope) = scope else {
        return if domain {
            Err("domain-extraction needs a scope (Start carries it from the preview)".to_string())
        } else {
            Ok(())
        };
    };
    if !domain {
        return Err(format!(
            "a scope is only for a {DOMAIN_EXTRACTION_WORKFLOW_ID} launch; this launch runs {}",
            workflow.unwrap_or("no preset")
        ));
    }
    let size = serde_json::to_vec(scope).map_or(usize::MAX, |b| b.len());
    if size > SCOPE_MAX_BYTES {
        return Err(format!("the scope is over {SCOPE_MAX_BYTES} bytes"));
    }
    let Some(obj) = scope.as_object() else {
        return Err("the scope is not a JSON object".to_string());
    };
    let extra: Vec<&str> = obj
        .keys()
        .map(String::as_str)
        .filter(|k| !SCOPE_KEYS.contains(k))
        .collect();
    if !extra.is_empty() {
        return Err(format!("the scope has unknown keys: {}", extra.join(", ")));
    }
    let filled = |v: &serde_json::Value| v.as_str().is_some_and(|s| !s.trim().is_empty());
    if !obj.get("intent").is_some_and(filled) {
        return Err("the scope has no intent (a non-empty string)".to_string());
    }
    if let Some(ph) = obj.get("phrases") {
        if !ph.as_array().is_some_and(|a| a.iter().all(filled)) {
            return Err("the scope's phrases are not a list of non-empty strings".to_string());
        }
    }
    for k in ["scope_hash", "description"] {
        if obj.get(k).is_some_and(|v| !v.is_string()) {
            return Err(format!("the scope's {k} is not a string"));
        }
    }
    Ok(())
}

/// (J6 C1b) Parse a launcher's scope JSON (core-ts `scopeJson`) without losing a field: an object
/// key that appears twice is refused, as garden's own reader refuses it, where a plain parse would
/// keep the last one and drop the first silently. Lone surrogate escapes and non-JSON text are
/// refused by the parser itself. The shape is then [`check_launch_scope`]'s.
pub fn parse_scope_json(text: &str) -> Result<serde_json::Value, String> {
    use serde::de::{Error as _, MapAccess, SeqAccess, Visitor};
    use serde_json::Value;
    struct Strict(Value);
    struct Strictly;
    impl<'de> serde::Deserialize<'de> for Strict {
        fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
            d.deserialize_any(Strictly).map(Strict)
        }
    }
    impl<'de> Visitor<'de> for Strictly {
        type Value = Value;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a JSON value")
        }
        fn visit_unit<E>(self) -> Result<Value, E> {
            Ok(Value::Null)
        }
        fn visit_bool<E>(self, v: bool) -> Result<Value, E> {
            Ok(Value::Bool(v))
        }
        fn visit_i64<E>(self, v: i64) -> Result<Value, E> {
            Ok(Value::from(v))
        }
        fn visit_u64<E>(self, v: u64) -> Result<Value, E> {
            Ok(Value::from(v))
        }
        fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Value, E> {
            serde_json::Number::from_f64(v)
                .map(Value::Number)
                .ok_or_else(|| E::custom("a number that is not finite"))
        }
        fn visit_str<E>(self, v: &str) -> Result<Value, E> {
            Ok(Value::String(v.to_owned()))
        }
        fn visit_string<E>(self, v: String) -> Result<Value, E> {
            Ok(Value::String(v))
        }
        fn visit_seq<A: SeqAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
            let mut out = Vec::new();
            while let Some(Strict(v)) = a.next_element()? {
                out.push(v);
            }
            Ok(Value::Array(out))
        }
        fn visit_map<A: MapAccess<'de>>(self, mut a: A) -> Result<Value, A::Error> {
            let mut out = serde_json::Map::new();
            while let Some(k) = a.next_key::<String>()? {
                if out.contains_key(&k) {
                    return Err(A::Error::custom(format!("a key appears twice: {k}")));
                }
                let Strict(v) = a.next_value()?;
                out.insert(k, v);
            }
            Ok(Value::Object(out))
        }
    }
    serde_json::from_str::<Strict>(text)
        .map(|s| s.0)
        .map_err(|e| format!("the scope is not JSON: {e}"))
}

/// The argv of a host step that runs a garden domain script on the run's repository: through the
/// garden generation the run was admitted against (`WICKED_GARDEN_ROOT`, handed to every generic
/// Tool unit — never a `wicked-garden` from the daemon's PATH), with the store and the repository
/// root as positional parameters so the planner binds the `{code_graph_db}` / `{repo_root}`
/// placeholders (whole arguments only, [`crate::plan::bind_repo_paths`]).
pub fn garden_domain_step_cmd(step: &str, script: &str, rest: &str) -> Vec<String> {
    vec![
        "bash".to_string(),
        "-c".to_string(),
        format!(
            "exec \"${{WICKED_GARDEN_ROOT:?the engine handed this Tool phase no garden root; refusing to resolve wicked-garden from PATH}}/scripts/wicked-garden\" run {script} --db \"$1\" --repo-root \"$2\" {rest}"
        ),
        step.to_string(),
        crate::workflow::CODE_GRAPH_DB_TOKEN.to_string(),
        crate::workflow::REPO_ROOT_TOKEN.to_string(),
    ]
}

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
/// is non-zero anywhere, AND (4) at least one requirement is genuinely resolved (`resolved >= 1`
/// in the top-level or any per-app entry), closing the gameable-by-RISK-placeholders case where
/// all behavior-bearing nodes receive RISK annotations and the model emits zero concrete rules
/// (FINDING-D1, 2026-08-12). Built only from `test`/`grep`/`!`/`${VAR:-fallback}` so it passes
/// the [`looks_dangerous`](crate::validator) denylist (no redirection, command substitution `$(`,
/// or destructive/network token; `${VAR}`/`${VAR:-default}` expansion and `!`/`||` are allowed).
/// The binary is invoked as `${WICKED_CORE_EXE:-wicked-core}`. The validator runner resolves and
/// injects that path (`execute_wrapped::resolve_wicked_core_exe_opt`) so the script finds the right
/// binary without relying on PATH. It used to inject `current_exe()`, which is the NODE binary when
/// the engine runs as a napi addon — so the script ran `node coverage` and the floor never executed
/// on a real run (FINDING-093).
///
/// MINIMUM, not 1.0 (operator ruling 2026-10-31): the recompute branch runs `wicked-core coverage
/// --min-coverage-pct`, which writes the report and EXITS NON-ZERO below the configured minimum
/// (`WICKED_REQUIREMENTS_MIN_COVERAGE_PCT`, injected by the validator runner from the daemon's
/// setting; the bare flag falls back to 80) — exact integer arithmetic in the binary, because grep
/// cannot compare a ratio to a number. The full-coverage greps moved INTO the no-store fallback
/// branch: without a store there is nothing to recompute, so a file alone still has to show 1.0.
pub const COVERAGE_SCRIPT: &str = r#"( test -n "${WICKED_COVERAGE_DB}" && "${WICKED_CORE_EXE:-wicked-core}" coverage --min-coverage-pct ) || ( test -z "${WICKED_COVERAGE_DB}" && test -f coverage-report.json && grep -Eq '"coverage":[[:space:]]*(1|1\.0+)([,}[:space:]]|$)' coverage-report.json && ! grep -Eq '"coverage":[[:space:]]*0' coverage-report.json && ! grep -Eq '"unaccounted":[[:space:]]*[1-9]' coverage-report.json && ! grep -Eq '"risk_flagged":[[:space:]]*[1-9]' coverage-report.json ) && test -f coverage-report.json && grep -Eq '"behavior_bearing":[[:space:]]*[1-9]' coverage-report.json && grep -Eq '"resolved":[[:space:]]*[1-9]' coverage-report.json"#;

/// The coverage report's filename, relative to the run's worktree.
///
/// Named once because three things have to agree on it AT RUNTIME: [`COVERAGE_SCRIPT`] (which reads
/// it and, on the fallback branch, causes it to be written), `wicked-core coverage` (which writes it
/// into its cwd), and the denial diagnostic that reads it back to report what was measured.
///
/// The preset's `coverage` step also lists the file in its
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

/// The APPROVED content-address pin the preset's `coverage` step carries.
/// Content-hash over `(COVERAGE_CRITERION, COVERAGE_SCRIPT, approved=true)` — see
/// [`crate::validator_vault::pin`]. Re-derived and asserted equal to the vaulted approved copy and to
/// the JSON's embedded pin by [`tests::embedded_pin_matches_the_approved_vaulted_validator`]; if the
/// criterion or script ever changes, that test fails loudly and this const must be regenerated.
pub const COVERAGE_VALIDATOR_PIN: &str = "9a2c71609c3e3ced";

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

/// Where the coverage minimum in force came from (e2e F29).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MinSource {
    /// The repository's own target in its store (`requirements-target --set`, which the person's
    /// "Lower the minimum" answer writes).
    Repository,
    /// The daemon-wide setting (`WICKED_REQUIREMENTS_MIN_COVERAGE_PCT`, else the default).
    Daemon,
}

impl MinSource {
    fn phrase(self) -> &'static str {
        match self {
            Self::Repository => "the repository's own target",
            Self::Daemon => "the daemon's setting",
        }
    }
}

/// The minimum `wicked-core coverage --min-coverage-pct` applies to the repository graph at
/// `graph_db` (e2e F29): its own target when one is set, else the daemon's setting — the same rule
/// as the binary's bare flag ([`wicked_governance::effective_min_coverage_pct`]), read through a
/// READ-ONLY connection (the store belongs to the run; nothing here writes it). An empty, in-memory
/// or URL path, or a store that cannot be opened, reads the daemon's setting. That is only the
/// number this note and the denial quote: with no store the script itself takes its file-only
/// branch (full coverage, no placeholders), and an unreadable store fails the script's recompute.
pub(crate) fn applied_min_coverage(graph_db: Option<&str>) -> Result<(u8, MinSource), String> {
    let usable = graph_db.filter(|p| !p.is_empty() && *p != ":memory:" && !p.contains("://"));
    if let Some(path) = usable {
        if let Ok(store) = wicked_apps_core::open_store_ro(Some(path)) {
            return match wicked_governance::repo_min_coverage_pct(&store) {
                Ok(Some(pct)) => Ok((pct, MinSource::Repository)),
                Ok(None) => {
                    wicked_governance::min_coverage_pct_from_env().map(|m| (m, MinSource::Daemon))
                }
                Err(e) => Err(format!(
                    "the repository's coverage target could not be read: {e}"
                )),
            };
        }
    }
    wicked_governance::min_coverage_pct_from_env().map(|m| (m, MinSource::Daemon))
}

/// The repository graph the coverage FLOOR measures for a governed unit (e2e F29): the operational
/// store (read-only) names the run's repository, and its graph is the repo-local one — the same
/// resolution as `actor::repo_code_graph_db`, never the project graph (a sibling repository's
/// target must not set this one's bar). `None` when the unit carries no repository or no graph.
///
/// The graph root comes from the operational store's own path
/// ([`crate::code_graph::repo_graph_root_for_store`]), not the calling thread's bound state home:
/// the judge runs on a runner thread, where the actor's binding is not set.
fn floor_graph_for(gov: &crate::workflow::GovernanceContext) -> Option<String> {
    let repo_id = gov.repo_id.as_deref()?;
    let store = wicked_apps_core::open_store_ro(Some(&gov.db_path)).ok()?;
    let repo = crate::repo::get_repo(&store, repo_id).ok().flatten()?;
    let root = crate::code_graph::repo_graph_root_for_store(&gov.db_path)?;
    let path = crate::code_graph::repo_graph_db_at(&root, &repo.graph_key);
    path.is_file().then(|| path.to_string_lossy().into_owned())
}

/// (e2e F29) The criterion a PINNED validator's agent judge applies:
/// [`crate::validator::pinned_judge_criterion`], plus, for the coverage validator, the minimum the
/// engine applies to this run ([`coverage_judge_note`]). The one call the judge block makes, so the
/// note cannot be dropped from one path (bus or inline) and not the other.
pub(crate) fn pinned_judge_criterion_for(
    v: &DeterministicValidator,
    gov: Option<&crate::workflow::GovernanceContext>,
) -> String {
    let base = crate::validator::pinned_judge_criterion(v);
    match coverage_judge_note(v, gov) {
        Some(note) => format!("{base}\n\n{note}"),
        None => base,
    }
}

/// (e2e F29) What the pinned coverage judge is told about the minimum, beside
/// [`COVERAGE_CRITERION`]: the number the engine applies to this run, read from the run's store
/// just before the judge runs, and that a minimum written in the work is not it. `None` for every
/// other validator. The floor still runs after the judge and denies below that minimum on its own;
/// this only stops the judge from rejecting on a stale number (run 1ec395da: the person lowered the
/// minimum to 66%, the floor passed, the judge rejected against "80%").
pub(crate) fn coverage_judge_note(
    v: &DeterministicValidator,
    gov: Option<&crate::workflow::GovernanceContext>,
) -> Option<String> {
    if v.criterion != COVERAGE_CRITERION || v.script != COVERAGE_SCRIPT {
        return None;
    }
    // No repository graph to read ⇒ no number: the floor may still resolve one the runner cannot
    // see (an in-memory operational store), and a guessed daemon default is the F29 bait again.
    let applied = gov
        .and_then(floor_graph_for)
        .ok_or_else(|| "the run's repository graph could not be resolved".to_string())
        .and_then(|g| applied_min_coverage(Some(&g)));
    Some(coverage_judge_note_for(applied))
}

/// [`coverage_judge_note`]'s text for a resolved (or unresolvable) minimum.
fn coverage_judge_note_for(applied: Result<(u8, MinSource), String>) -> String {
    let head = match applied {
        Ok((pct, source)) => format!(
            "[applied minimum] The engine holds this run's real-requirements share to {pct}% ({}), \
             read from the run's store just before this review.",
            source.phrase()
        ),
        Err(_) => "[applied minimum] The minimum is the one the engine applies from the run's \
                   store (the repository's own target when one is set, else the daemon's \
                   setting); it could not be read for this note."
            .to_string(),
    };
    format!(
        "{head} A different minimum, share or `minimum_met` written in the WORK (an earlier page's \
         report, a log line from before the person changed the target) is stale and decides \
         nothing: do not reject on it. The engine's floor re-measures the share against this \
         minimum after your verdict."
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validator::run_validator;
    use crate::validator_vault::{load_validator, pin};
    use crate::workflow::GateSpec;
    use crate::{domain::StageKind, plan::plan_from_def};

    /// The shipped def: the built-in preset as the engine composes it (X-MIG M6/M11).
    fn load_shipped_def() -> crate::workflow::WorkflowDef {
        crate::preset::builtin_preset_def(DOMAIN_EXTRACTION_WORKFLOW_ID)
    }

    /// (J6 v3) The shipped preset is two host steps, `domain-run` then `domain-graph`, with no
    /// seat unit, no skill, no gate type, no human gate and no validator pin: a run launched with
    /// no human confirm ends with no gate. Fails on the old 9-step preset.
    #[test]
    fn the_shipped_preset_is_two_host_steps_with_no_gate() {
        let def = load_shipped_def();
        assert_eq!(def.id, DOMAIN_EXTRACTION_WORKFLOW_ID);
        let ids: Vec<&str> = def.phases.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, [crate::catalog::DOMAIN_RUN_STEP, "domain-graph"]);
        def.validate().expect("shipped def is a valid WorkflowDef");
        for p in &def.phases {
            assert!(
                matches!(p.executor, crate::workflow::PhaseExecutor::Tool { .. }),
                "{} is a host step",
                p.id
            );
            assert!(p.skill_ref.is_none(), "{} runs no skill", p.id);
            assert_eq!(p.gate_type, None, "{} has no gate type", p.id);
            assert!(
                !matches!(
                    p.gate,
                    GateSpec::HumanConfirm { .. } | GateSpec::HumanConfirmIf(_)
                ),
                "{} has no human gate: {:?}",
                p.id,
                p.gate
            );
            assert!(p.validator_pin.is_none(), "{} carries no pin", p.id);
        }
        let dg = &def.phases[1];
        assert_eq!(dg.depends_on, [crate::catalog::DOMAIN_RUN_STEP]);
        assert_eq!(dg.kind, StageKind::Build);
        let units = plan_from_def(&def, "map the payments service", "s1");
        assert_eq!(units.len(), 2);
        assert!(units
            .iter()
            .all(|u| u.tool_cmd.is_some() && u.skill_ref.is_none()));
    }

    /// (J6 C1b) `domain-run` runs garden's `run.py` from the admitted garden root and reads the
    /// scope file core wrote into the step's channel directory, refusing by name when there is
    /// none. `domain-graph` builds into the repository's store (`--db`) for the checkout
    /// (`--repo-root`) and writes no file into the run.
    #[test]
    fn domain_run_reads_the_scope_core_hands_it_and_domain_graph_builds_into_the_store() {
        let def = load_shipped_def();
        let cmd = |i: usize| match &def.phases[i].executor {
            crate::workflow::PhaseExecutor::Tool { cmd } => cmd.clone(),
            other => panic!("not a Tool: {other:?}"),
        };
        let run = cmd(0);
        assert_eq!(run[..2], ["bash", "-c"]);
        assert!(
            run[2].contains("run scripts/domain/run.py --db \"$1\" --repo-root \"$2\" --scope \"${WICKED_DOMAIN_SCOPE:?"),
            "{run:?}"
        );
        assert!(run[2].contains("${WICKED_GARDEN_ROOT:?"), "{run:?}");
        assert_eq!(
            run[3..],
            [
                crate::catalog::DOMAIN_RUN_STEP,
                crate::workflow::CODE_GRAPH_DB_TOKEN,
                crate::workflow::REPO_ROOT_TOKEN
            ]
        );
        assert_eq!(crate::model_call::SCOPE_ENV, "WICKED_DOMAIN_SCOPE");
        let graph = cmd(1);
        assert_eq!(
            graph,
            [
                "wicked-core",
                "domain-graph",
                "--db",
                crate::workflow::CODE_GRAPH_DB_TOKEN,
                "--repo-root",
                crate::workflow::REPO_ROOT_TOKEN
            ]
        );
    }

    /// (J6 C1b, codex r1) A launcher's scope text parses without losing a field: a key twice, at
    /// the top or nested, is refused by name, as garden refuses it; a lone surrogate and non-JSON
    /// are refused; a well-formed scope parses to the same value a plain parse gives.
    #[test]
    fn a_scope_with_a_key_twice_or_bad_text_is_refused_at_parse() {
        for (text, why) in [
            (
                r#"{"intent":"a","scope_hash":"h","scope_hash":""}"#,
                "a key appears twice: scope_hash",
            ),
            (
                r#"{"intent":"a","intent":"b"}"#,
                "a key appears twice: intent",
            ),
            (
                r#"{"intent":"a","phrases":["x"],"phrases":[]}"#,
                "a key appears twice: phrases",
            ),
            (
                r#"{"intent":"a","x":{"k":1,"k":2}}"#,
                "a key appears twice: k",
            ),
            (r#"{"intent":"\ud800"}"#, "the scope is not JSON"),
            ("not json", "the scope is not JSON"),
        ] {
            let err = parse_scope_json(text).unwrap_err();
            assert!(err.contains(why), "{text}: {err}");
        }
        let text = r#"{"intent":"refunds","phrases":["remboursé"],"n":[1,-2,2.5,null,true]}"#;
        assert_eq!(
            parse_scope_json(text).unwrap(),
            serde_json::from_str::<serde_json::Value>(text).unwrap()
        );
    }

    /// (J6 C1b, coord ruling B') The launch's scope check, refusal by refusal. Each case fails
    /// without its clause.
    #[test]
    fn a_launch_scope_is_checked_and_each_refusal_names_why() {
        use serde_json::json;
        let de = Some(DOMAIN_EXTRACTION_WORKFLOW_ID);
        let ok = json!({"intent": "payments", "phrases": ["refund"], "scope_hash": "ab", "description": "d"});
        assert_eq!(check_launch_scope(de, Some(&ok)), Ok(()));
        assert_eq!(
            check_launch_scope(de, Some(&json!({"intent": "x"}))),
            Ok(())
        );
        // No scope: refused on domain-extraction, fine elsewhere.
        assert_eq!(
            check_launch_scope(de, None).unwrap_err(),
            "domain-extraction needs a scope (Start carries it from the preview)"
        );
        assert_eq!(check_launch_scope(Some("feature"), None), Ok(()));
        assert_eq!(check_launch_scope(None, None), Ok(()));
        // A scope on another launch is refused, never dropped.
        let other = check_launch_scope(Some("feature"), Some(&ok)).unwrap_err();
        assert!(
            other.contains("only for a domain-extraction launch") && other.contains("feature"),
            "{other}"
        );
        assert!(check_launch_scope(None, Some(&ok))
            .unwrap_err()
            .contains("no preset"));
        for (bad, why) in [
            (json!("payments"), "the scope is not a JSON object"),
            (json!(["payments"]), "the scope is not a JSON object"),
            (
                json!({"intent": "x", "extra": 1, "more": 2}),
                "the scope has unknown keys: extra, more",
            ),
            (json!({}), "the scope has no intent (a non-empty string)"),
            (
                json!({"intent": "  "}),
                "the scope has no intent (a non-empty string)",
            ),
            (
                json!({"intent": 3}),
                "the scope has no intent (a non-empty string)",
            ),
            (
                json!({"intent": null}),
                "the scope has no intent (a non-empty string)",
            ),
            (
                json!({"intent": "x", "phrases": null}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "phrases": "refund"}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "phrases": ["refund", " "]}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "phrases": [1]}),
                "the scope's phrases are not a list of non-empty strings",
            ),
            (
                json!({"intent": "x", "scope_hash": 1}),
                "the scope's scope_hash is not a string",
            ),
            (
                json!({"intent": "x", "description": null}),
                "the scope's description is not a string",
            ),
        ] {
            assert_eq!(
                check_launch_scope(de, Some(&bad)).unwrap_err(),
                why,
                "{bad}"
            );
        }
        // The size bound is on the compact JSON: one byte over refuses, the bound itself passes.
        let pad = |n: usize| json!({"intent": "x", "description": "d".repeat(n)});
        let base = serde_json::to_vec(&pad(0)).unwrap().len();
        assert_eq!(
            check_launch_scope(de, Some(&pad(SCOPE_MAX_BYTES - base))),
            Ok(())
        );
        assert_eq!(
            check_launch_scope(de, Some(&pad(SCOPE_MAX_BYTES - base + 1))).unwrap_err(),
            "the scope is over 65536 bytes"
        );
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

    /// (e2e F29) The criterion is also the pinned judge's text, so it names no number and no
    /// default: run 1ec395da's judge rejected a repository the person had lowered to 66% against the
    /// "default 80%" written here. Fails on the old text.
    #[test]
    fn the_criterion_names_no_fixed_minimum_and_lets_the_recompute_decide() {
        let c = COVERAGE_CRITERION;
        assert!(!c.contains("80"), "{c}");
        assert!(!c.contains("default"), "{c}");
        assert!(!c.contains("WICKED_REQUIREMENTS_MIN_COVERAGE_PCT"), "{c}");
        assert!(
            c.contains("the engine recomputes from the run's store"),
            "{c}"
        );
        assert!(c.contains("the repository's own target"), "{c}");
        assert!(c.contains("written in the work does not"), "{c}");
        assert!(c.contains("resolved > 0"), "{c}");
    }

    /// (e2e F29, codex r1) The production path: a governed unit's judge criterion
    /// ([`pinned_judge_criterion_for`], the one call the judge block makes) resolves the run's
    /// REGISTERED repository from the operational store, reads its repo-local graph under the
    /// daemon's repo-graph root (from the store path, not a thread binding) and carries 66%.
    #[cfg(unix)] // a git fixture, like the suite's other repo-registration tests
    #[test]
    fn a_governed_coverage_judge_is_told_the_repository_target() {
        use wicked_apps_core::spawn::HardenedCommand;
        // The graph root reads a process-wide override another test sets under the write lock.
        let _env = crate::code_graph::REPO_GRAPH_ROOT_ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("wicked-domainext-gov-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let repo = dir.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("README.md"), "x\n").unwrap();
        for args in [
            &["init", "-q", "-b", "main", "."][..],
            &[
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "add",
                "-A",
            ],
            &[
                "-c",
                "user.email=t@example.invalid",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "init",
            ],
        ] {
            // spawn-audit: hardened git fixture (a throwaway repo under a temp dir).
            let ok = std::process::Command::new("git")
                .hardened()
                .args(args)
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .status()
                .unwrap();
            assert!(ok.success(), "git {args:?}");
        }
        let db = dir.join("core.db").to_string_lossy().into_owned();
        let entry = {
            let mut store = wicked_apps_core::open_store(Some(&db)).unwrap();
            crate::repo::register_repo(
                &mut store,
                crate::repo::RepoSpec {
                    name: "tiny-math".into(),
                    root_path: repo.to_string_lossy().into_owned(),
                    registered_at: 0,
                },
            )
            .unwrap()
        };
        let graph = crate::code_graph::repo_graph_db_at(
            &crate::code_graph::repo_graph_root_for_store(&db).expect("a graph root"),
            &entry.graph_key,
        );
        std::fs::create_dir_all(graph.parent().unwrap()).unwrap();
        {
            let mut g = wicked_apps_core::open_store(Some(graph.to_str().unwrap())).unwrap();
            wicked_governance::set_repo_min_coverage_pct(&mut g, Some(66)).unwrap();
        }
        let gov = |repo_id: Option<String>| crate::workflow::GovernanceContext {
            human_confirm: Default::default(),
            db_path: db.clone(),
            code_graph_db: None,
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            optional_read_roots: Vec::new(),
            project_id: None,
            repo_id,
            apps: None,
        };
        let v = coverage_eq_one_validator();
        let c = pinned_judge_criterion_for(&v, Some(&gov(Some(entry.id.clone()))));
        assert!(c.starts_with(COVERAGE_CRITERION), "{c}");
        assert!(c.contains("to 66% (the repository's own target)"), "{c}");
        assert!(!c.contains("80%"), "{c}");
        // No repository on the unit: the note says so, with no number.
        let blind = pinned_judge_criterion_for(&v, Some(&gov(None)));
        assert!(blind.contains("could not be read for this note"), "{blind}");
        // Any other pinned validator: the criterion is exactly the floor contract, no note.
        let other = DeterministicValidator {
            criterion: "lint passes".into(),
            script: "true".into(),
            approved: true,
        };
        assert_eq!(
            pinned_judge_criterion_for(&other, Some(&gov(Some(entry.id)))),
            crate::validator::pinned_judge_criterion(&other)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// (e2e F29) The judge is told the minimum the engine APPLIES: the repository's own target read
    /// from its store (66 after "Lower the minimum"), not the daemon's 80. Fails without the fix:
    /// the judge's text carried only the criterion.
    #[test]
    fn the_judge_note_carries_the_minimum_applied_from_the_store() {
        let dir = std::env::temp_dir().join(format!("wicked-domainext-min-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("graph.db");
        {
            let mut store = wicked_apps_core::open_store(Some(db.to_str().unwrap())).unwrap();
            wicked_governance::set_repo_min_coverage_pct(&mut store, Some(66)).unwrap();
        }
        let applied = applied_min_coverage(db.to_str());
        assert_eq!(applied, Ok((66, MinSource::Repository)));
        let note = coverage_judge_note_for(applied);
        assert!(note.contains("66%"), "{note}");
        assert!(note.contains("the repository's own target"), "{note}");
        assert!(note.contains("is stale and decides nothing"), "{note}");
        // The whole judge prompt: criterion + floor contract + the applied minimum, and no 80.
        let v = coverage_eq_one_validator();
        let prompt = format!("{}\n\n{note}", crate::validator::pinned_judge_criterion(&v));
        assert!(
            prompt.contains("66%") && !prompt.contains("80%"),
            "{prompt}"
        );
        // Unresolvable store: no number at all (never a guessed default).
        let blind = coverage_judge_note_for(Err("no graph".into()));
        assert!(!blind.chars().any(|c| c.is_ascii_digit()), "{blind}");
        // Only the coverage validator gets a note.
        assert!(coverage_judge_note(&v, None).is_some());
        let other = DeterministicValidator {
            criterion: "lint passes".into(),
            script: "true".into(),
            approved: true,
        };
        assert!(coverage_judge_note(&other, None).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn embedded_pin_matches_the_approved_vaulted_validator() {
        // The load-bearing tie: the embedded COVERAGE_VALIDATOR_PIN == the pin of the
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
        // (J6 C1b) The shipped preset no longer pins it; C2 retires this closure.

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

    /// The recompute branch asks the binary for the CONFIGURED MINIMUM (operator ruling 2026-10-31):
    /// a recompute that writes a report and exits non-zero (the binary's below-minimum exit 3) must
    /// deny, even though the report it wrote would satisfy every grep that follows. Mutation: drop
    /// `--min-coverage-pct` from the script, or put the `||` fallback back in front of the greps, and
    /// the below-minimum stub passes.
    #[test]
    fn coverage_gate_denies_a_recompute_below_the_configured_minimum() {
        use wicked_apps_core::HardenedCommand;
        assert!(
            COVERAGE_SCRIPT.contains("coverage --min-coverage-pct"),
            "the recompute is a minimum check"
        );
        let dir = std::env::temp_dir().join(format!("cov_belowmin_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Writes a substantive report (passes the trailing greps), then exits 3 = below the minimum.
        // It also records its argv, so the test sees the flag reach the binary.
        let stub = dir.join("stub-belowmin.sh");
        std::fs::write(
            &stub,
            "#!/bin/sh\necho \"$@\" > argv.txt\ncat > coverage-report.json <<'JSON'\n{\"total\":900,\"behavior_bearing\":42,\"resolved\":20,\"risk_flagged\":0,\"unaccounted\":22,\"coverage\":0.4762}\nJSON\nexit 3\n",
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
            .env(crate::gate_hook::COVERAGE_DB_ENV, dir.join("repo.db"))
            .env(crate::gate_hook::WICKED_CORE_EXE_ENV, &stub)
            .status()
            .map(|st| st.success())
            .unwrap_or(false);
        assert!(!ok, "a recompute below the configured minimum denies");
        let argv = std::fs::read_to_string(dir.join("argv.txt")).unwrap();
        assert_eq!(argv.trim(), "coverage --min-coverage-pct");
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
  "resolved": 30,
  "risk_flagged": 0,
  "unaccounted": 0,
  "coverage": 1.0,
  "resolve_threshold": 0.75,
  "unaccounted_nodes": []
}"#,
        )
        .unwrap();
        assert!(
            run_validator(&approved, &pass_wt).unwrap(),
            "every behaviour a real requirement ⇒ gate PASSES"
        );

        // core#1233 (codex on #1234): without a store, placeholders never count either. 28 real +
        // 2 placeholders is fully ACCOUNTED (coverage 1.0, unaccounted 0) and used to pass.
        let placeholder_wt = base.join("placeholders");
        std::fs::create_dir_all(&placeholder_wt).unwrap();
        std::fs::write(
            placeholder_wt.join("coverage-report.json"),
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
            !run_validator(&approved, &placeholder_wt).unwrap(),
            "placeholders do not count toward the no-store fallback ⇒ gate FAILS"
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

        // SUBSTANCE CHECK — the RISK-placeholder false-pass (D1 substance fix). An extraction
        // where every behavior-bearing node has a RISK placeholder produces coverage == 1.0 and
        // unaccounted == 0 (all nodes accounted for), but resolved == 0 (no real rules emitted).
        // The pre-D1 gate accepted this; the new `resolved >= 1` check rejects it.
        let risk_wt = base.join("risk_only");
        std::fs::create_dir_all(&risk_wt).unwrap();
        std::fs::write(
            risk_wt.join("coverage-report.json"),
            r#"{
  "total": 30,
  "behavior_bearing": 20,
  "resolved": 0,
  "risk_flagged": 20,
  "unaccounted": 0,
  "coverage": 1.0,
  "resolve_threshold": 0.75,
  "unaccounted_nodes": []
}"#,
        )
        .unwrap();
        assert!(
            !run_validator(&approved, &risk_wt).unwrap(),
            "coverage == 1.0 but resolved == 0 (all RISK placeholders) ⇒ gate FAILS (substance check)"
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
