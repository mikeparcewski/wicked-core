//! core#28 — the whole thing runs end-to-end with governance ENFORCED (DES-OUTGOV-007).
//!
//! Drives the `domain-extraction` workflow through the real engine against a hand-seeded estate store
//! at coverage 1.0, and proves the payoff:
//!   TEST 1 — a governed run produces the gated artifacts: `coverage-report.json` (recomputed FROM the
//!            store by the real `wicked-core coverage`) clears the pinned coverage validator, and
//!            `wicked-core domain-graph` writes `requirements_graph.json`; the run completes.
//!   TEST 2 — a POLICY violation in a phase's OUTPUT denies that phase (deny-dominates), the run fails,
//!            and no downstream `requirements_graph.json` is produced.
//!   TEST 3 — a registered CONFORMANCE RULE is RECALLED into the run: the per-unit output claim carries
//!            the rule as an obligation (the M6/M7 recall→gate wiring firing IN the loop — inert before
//!            this milestone).
//!
//! Unix-gated: the pinned coverage validator is a POSIX grep script (`domain_extraction.rs`).
#![cfg(unix)]

use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_apps_core::{
    open_store, synthetic_symbol, GraphRead, GraphWrite, Language, Location, Node, NodeKind, Span,
    CONFORMANCE_CLAIM, SYMBOL_SCHEME,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};
use wicked_estate_core::query::SymbolQuery;
use wicked_estate_core::Annotation;
use wicked_governance::{
    register_policy, register_rule, ConfSeverity, ConformanceRule, Effect, Policy, RuleProvenance,
    RuleType, Severity, Targets, Trigger,
};

use wicked_core::{
    provision_and_approve_coverage_validator, Core, EntityMode, HumanConfirm, HumanDecision,
    LaunchSpec, RepoSpec, SessionStatus, StepInput, StepOutput, StepRunner, StepStatus, UnitStatus,
};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

// --- reused fixtures (mirrors seam_findings.rs / coverage_cli.rs / p3_repo.rs) ---

struct StubDispatcher;
impl Dispatcher for StubDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "x".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "stub".into(),
        })
    }
}

fn cli(key: &str) -> AgenticCli {
    AgenticCli {
        key: key.into(),
        display_name: key.into(),
        binary: "unused".into(),
        headless_invocation: "unused {PROMPT}".into(),
        category: Category::default(),
        input_mode: InputMode::default(),
        version_probe: vec![],
        trust_flags: vec![],
        alt_binaries: vec![],
        confidence: Confidence::default(),
        enabled_for_council: true,
    }
}

/// A deny policy scoped EXACTLY to one phase (`applies_to == [phase]`).
fn deny_policy(phase: &str, pattern: &str) -> Policy {
    Policy {
        id: format!("deny-{phase}"),
        kind: "guard".into(),
        applies_to: vec![phase.into()],
        effect: Effect::Deny,
        trigger: Trigger {
            contains: Some(pattern.into()),
        },
        obligations: vec![],
        criteria: String::new(),
        severity: Severity::High,
        rule: "deny".into(),
    }
}

fn is_terminal(s: SessionStatus) -> bool {
    matches!(
        s,
        SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
    )
}

fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> bool {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return true;
                }
                // Fail fast: a terminal status that isn't the one we want will never change.
                if want != v.session.status && is_terminal(v.session.status) {
                    return false;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    false
}

/// Seed ONE behavior node. `accounted` ⇒ a validated requirement + a business-rule annotation so the
/// store recomputes to front-half coverage 1.0; `!accounted` ⇒ a BARE function → an unaccounted hole →
/// coverage < 1.0 (so the pinned coverage validator denies).
fn seed(db: &str, accounted: bool) {
    let mut store = open_store(Some(db)).unwrap();
    let n = Node::new(
        synthetic_symbol("code", "charge"),
        NodeKind::Function,
        "charge".to_string(),
        Language::new(SYMBOL_SCHEME),
        Location::new("billing/charge.rs".to_string(), Span::ZERO),
    );
    store.begin_batch().unwrap();
    store.upsert_nodes(std::slice::from_ref(&n)).unwrap();
    store.commit_batch().unwrap();
    if accounted {
        store
            .set_node_semantics(&n.symbol, None, Some("REQ-1"), Some(true))
            .unwrap();
        store
            .annotate(
                &n.symbol,
                Annotation::new("business_rule", "r", "amount > 0").with_confidence(0.9),
            )
            .unwrap();
    }
}

fn make_git_repo(name: &str) -> std::path::PathBuf {
    let repo = std::env::temp_dir().join(format!("wicked-core-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(args)
            .output()
            .unwrap()
    };
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("README.md"), "hello").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    repo
}

fn db_in(name: &str) -> String {
    let dir =
        std::env::temp_dir().join(format!("wicked-core-e2e-db-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

/// The domain-extraction runner: keys on the unit's ORD. Units 1-3 emit benign output; the coverage
/// phase (ord 4) shells the REAL `wicked-core coverage` (recompute FROM the seeded store → the pinned
/// coverage validator greps `coverage-report.json` in the worktree); the domain-graph phase (ord 5)
/// shells the REAL `wicked-core domain-graph` → writes `requirements_graph.json`. `out_override` lets a
/// test inject a policy-tripping output for a specific ord.
struct DomainExtractionRunner {
    db: String,
    out_override: Option<(u32, String)>,
}
impl StepRunner for DomainExtractionRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        // The engine's semantic agent-judge (validator-pinned phases) routes its PASS/REJECT review
        // THROUGH this runner as a `validator-agent` unit. Emit a clean PASS so the agent gate clears —
        // the DETERMINISTIC coverage script (grepping coverage-report.json) is the real coverage check.
        if i.unit.id == "validator-agent" {
            return StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: "PASS\nthe work meets the criterion".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                governed: false,
            };
        }
        let ord = i.unit.ord;
        let workdir = i.workdir.clone();
        let mut output = format!("unit-{ord} done");
        if let Some((o, ref text)) = self.out_override {
            if o == ord {
                output = text.clone();
            }
        }
        if let Some(wd) = workdir.as_ref() {
            // Surface any subprocess failure LOUDLY (stderr + exit) so a lock / missing-binary / CLI
            // error can't silently mask itself as a later "file missing" — never `let _ = …output()`.
            let shell = |args: &[&str], label: &str| match Command::new(BIN).args(args).output() {
                Ok(o) if o.status.success() => {}
                Ok(o) => eprintln!(
                    "e2e runner: `{label}` exited {}: {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr)
                ),
                Err(e) => eprintln!("e2e runner: `{label}` failed to spawn: {e}"),
            };
            if ord == 4 {
                let out = wd.join("coverage-report.json");
                shell(
                    &["coverage", "--db", &self.db, "--out", out.to_str().unwrap()],
                    "coverage",
                );
            } else if ord == 5 {
                let out = wd.join("requirements_graph.json");
                shell(
                    &[
                        "domain-graph",
                        "--db",
                        &self.db,
                        "--out",
                        out.to_str().unwrap(),
                    ],
                    "domain-graph",
                );
            }
        }
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output,
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            governed: false,
        }
    }
}

/// Common setup: a store seeded to `accounted` coverage (1.0 when true, a hole when false) with the
/// pinned coverage validator approved, a registered git repo, the drop-in workflow dir on the resolver
/// path, and a spawned Core. Returns (core, repo_entry_id, db, repo_root).
fn setup(
    name: &str,
    runner: DomainExtractionRunner,
    accounted: bool,
) -> (Core, String, String, std::path::PathBuf) {
    let db = runner.db.clone();
    seed(&db, accounted);
    {
        let mut store = open_store(Some(&db)).unwrap();
        provision_and_approve_coverage_validator(&mut store).unwrap();
    }
    // domain-extraction is an operator drop-in, not a built-in — point the resolver at repo/workflows.
    // `set_var` is a data race under parallel test threads: set it EXACTLY ONCE (every caller sets the
    // same value, so a single init is correct).
    static WORKFLOWS_DIR_INIT: std::sync::Once = std::sync::Once::new();
    WORKFLOWS_DIR_INIT.call_once(|| {
        std::env::set_var(
            "WICKED_WORKFLOWS_DIR",
            format!("{}/workflows", env!("CARGO_MANIFEST_DIR")),
        );
    });
    let repo = make_git_repo(name);
    let core = Core::spawn_with_engine(db.clone(), Arc::new(StubDispatcher), Arc::new(runner));
    let entry = core
        .register_repo(RepoSpec {
            name: name.into(),
            root_path: repo.to_str().unwrap().into(),
            registered_at: 0,
        })
        .expect("register repo");
    (core, entry.id, db, repo)
}

fn launch(core: &Core, run_id: &str, repo_ref: &str) {
    core.launch_run(LaunchSpec {
        problem: "extract the domain model".into(),
        clis: vec![cli("a")],
        entity_mode: EntityMode::Shared,
        session_id: run_id.into(),
        human_confirm: HumanConfirm::None,
        repo_ref: Some(repo_ref.into()),
        workflow: Some("domain-extraction".into()),
    })
    .expect("launch domain-extraction");
}

/// Derive the run's worktree from the repo root `setup` returns (not by re-deriving the temp-path
/// scheme — so a change to the repo naming can't silently break this).
fn worktree(repo_root: &std::path::Path, run_id: &str) -> std::path::PathBuf {
    repo_root.join(".wicked").join("worktrees").join(run_id)
}

// --- TEST 1: the governed run produces the gated artifacts ---

#[test]
fn a_governed_run_produces_coverage_and_requirements_graph() {
    let db = db_in("happy");
    let (core, repo_id, _db, repo) = setup(
        "happy",
        DomainExtractionRunner {
            db,
            out_override: None,
        },
        true,
    );
    launch(&core, "run-happy", &repo_id);

    // The domain-graph phase carries a human-confirm gate → the run parks awaiting a human.
    assert!(
        wait_status(&core, "run-happy", SessionStatus::AwaitingHuman),
        "the run reaches the domain-graph human-confirm gate"
    );
    let wt = worktree(&repo, "run-happy");
    assert!(
        wt.join("coverage-report.json").is_file(),
        "the coverage phase wrote a store-recomputed coverage-report.json into the worktree"
    );
    assert!(
        wt.join("requirements_graph.json").is_file(),
        "the domain-graph phase produced requirements_graph.json"
    );

    // Approve the human gate → the run completes.
    core.confirm_gate("run-happy", HumanDecision::Approve { amend: None })
        .expect("approve the domain-graph gate");
    assert!(
        wait_status(&core, "run-happy", SessionStatus::Completed),
        "approving the final gate completes the governed run"
    );
}

// --- TEST 2: a policy violation in a phase's output denies the phase ---

#[test]
fn a_policy_violation_denies_a_phase_and_halts_the_run() {
    let db = db_in("deny");
    // Trip a deny on the extractor phase (ord 3 → phase `unit-3`) via a token in its output.
    let (core, repo_id, dbp, repo) = setup(
        "deny",
        DomainExtractionRunner {
            db,
            out_override: Some((3, "emitting LEAKTOKEN in the output".into())),
        },
        true,
    );
    {
        let mut store = open_store(Some(&dbp)).unwrap();
        register_policy(&mut store, &deny_policy("unit-3", "LEAKTOKEN")).unwrap();
    }
    launch(&core, "run-deny", &repo_id);

    assert!(
        wait_status(&core, "run-deny", SessionStatus::Failed),
        "a policy violation in the extractor phase's output denies it → the run fails"
    );
    // Attribute the failure to the EXTRACTOR phase (ord 3 / unit_ix 2) specifically — not an unrelated
    // gate — so the test proves the policy-over-output deny, not just "some failure".
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "run-deny").unwrap();
    assert_eq!(
        v.units[2].status,
        UnitStatus::Rejected,
        "the extractor phase (ord 3) is the denied unit"
    );
    let wt = worktree(&repo, "run-deny");
    assert!(
        !wt.join("requirements_graph.json").is_file(),
        "the run halted at the denied phase — no downstream requirements_graph.json"
    );
}

// --- TEST 3: a conformance rule is recalled into the run as an obligation ---

#[test]
fn a_conformance_rule_is_recalled_onto_the_run_claims() {
    let db = db_in("recall");
    let (core, repo_id, dbp, _repo) = setup(
        "recall",
        DomainExtractionRunner {
            db,
            out_override: None,
        },
        true,
    );
    {
        let mut store = open_store(Some(&dbp)).unwrap();
        register_rule(
            &mut store,
            &ConformanceRule {
                id: "PAT-777".into(),
                rule_type: RuleType::Pattern,
                statement: "no plaintext secrets in output".into(),
                severity: ConfSeverity::Critical,
                confidence: 0.95,
                targets: Targets::default(),
                symbol_ref: None,
                compliance: None,
                provenance: RuleProvenance::default(),
            },
        )
        .unwrap();
    }
    launch(&core, "run-recall", &repo_id);
    assert!(
        wait_status(&core, "run-recall", SessionStatus::AwaitingHuman),
        "the run reaches the domain-graph gate"
    );

    // The recall→gate wiring fires per unit: at least one persisted conformance claim carries the rule
    // as an obligation. Before this milestone, no run claim ever carried a recalled rule.
    let store = open_store(Some(&dbp)).unwrap();
    let claims = store
        .find_symbols(&SymbolQuery {
            kinds: vec![NodeKind::Other(CONFORMANCE_CLAIM.to_string())],
            ..Default::default()
        })
        .unwrap();
    let has_obligation = claims.iter().any(|c| {
        c.metadata
            .get("obligations")
            .and_then(|o| o.as_array())
            .is_some_and(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str())
                    .any(|s| s.contains("PAT-777"))
            })
    });
    assert!(
        has_obligation,
        "a registered conformance rule is recalled as an obligation on a run's output claim (M6/M7 \
         wiring live in the loop). claims found: {}",
        claims.len()
    );

    let _ = core.cancel_run("run-recall");
}

// --- TEST 4: a coverage HOLE is DENIED by the pinned coverage validator IN A RUN ---

/// The enforcement proof (vs TEST 1's happy path): seed a store BELOW full coverage so the coverage
/// phase's recomputed report is < 1.0, and assert the pinned coverage validator DENIES it in the run —
/// so the gate genuinely has teeth (a regression that disconnected the pinned validator would let this
/// through). Also proves the agent-judge PASS shim does NOT rescue a coverage hole: the DETERMINISTIC
/// validator's deny dominates the agent's PASS.
#[test]
fn a_coverage_hole_is_denied_by_the_pinned_validator_in_a_run() {
    let db = db_in("hole");
    let (core, repo_id, _db, repo) = setup(
        "hole",
        DomainExtractionRunner {
            db,
            out_override: None,
        },
        false, // a BARE function → an unaccounted hole → coverage < 1.0
    );
    launch(&core, "run-hole", &repo_id);

    // Poll until the coverage phase (ord 4 / unit_ix 3) is REJECTED — the deterministic coverage
    // validator denied the sub-1.0 report. A not-pass verdict on the `human_confirm_if verdict_not_pass`
    // coverage gate escalates rather than hard-failing, so assert on the UNIT, not the session status.
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut rejected = false;
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == "run-hole") {
                if v.units.get(3).map(|u| u.status) == Some(UnitStatus::Rejected) {
                    rejected = true;
                    break;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(
        rejected,
        "the pinned coverage validator DENIES a sub-1.0 coverage report in a run (the gate has teeth; \
         the agent-PASS shim does not rescue a hole)"
    );
    let wt = worktree(&repo, "run-hole");
    // The report WAS written (< 1.0) — so the deny is the sub-1.0 coverage path, not a missing-file
    // rejection that would pass for the wrong reason.
    assert!(
        wt.join("coverage-report.json").is_file(),
        "the coverage phase produced a report; the rejection is a genuine sub-1.0 deny, not a missing file"
    );
    assert!(
        !wt.join("requirements_graph.json").is_file(),
        "the run halted at the denied coverage phase — the domain-graph artifact was never produced"
    );

    let _ = core.cancel_run("run-hole");
}
