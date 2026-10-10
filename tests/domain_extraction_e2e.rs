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
use wicked_estate_core::ValidationClaim;
use wicked_governance::{
    register_policy, register_rule, ConfSeverity, ConformanceRule, Effect, Policy, RuleProvenance,
    RuleType, Severity, Targets, Trigger,
};

use wicked_core::{
    provision_and_approve_coverage_validator, Core, CoreEvent, EntityMode, HumanConfirm,
    HumanDecision, LaunchSpec, RepoSpec, SessionStatus, SessionView, StepInput, StepOutput,
    StepRunner, StepStatus, UnitStatus,
};

/// The crew-shaped skills snapshot fixture (shared with `skills_plan_admission.rs`): the
/// domain-extraction workflow's units carry `skill_ref`s, and the plan-wide skills admission runs
/// before the run's first unit — on a hermetic runner there is no snapshot and no live garden
/// cache, so without this fixture the run is refused by name (core#396 review pass 7; the test
/// used to pass locally only because the fallback found the developer's installed garden).
#[path = "support/skills_snapshot_fixture.rs"]
mod skills_fixture;

/// The skills the `domain-extraction` preset's plan names: the three domain skills. The security
/// reviewer's stays in the fixture although the plan no longer carries `security_review` (a
/// non-code run owes none, core#649 / #847: the coverage judge is a self-verifying evaluator, so
/// the run is not code work); an extra skill in the snapshot admits nothing the plan lacks.
const DOMAIN_EXTRACTION_SKILLS: &[&str] = &[
    "wicked-garden-domain",
    "wicked-garden-domain-extractor",
    "wicked-garden-domain-coverage",
    "wicked-garden-qe-security-test-engineer",
];

/// The fixture generation `setup` published once for this process — the value
/// `WICKED_SKILLS_SNAPSHOT` carries — for asserting the run was admitted against exactly it.
fn skills_snapshot_root() -> std::path::PathBuf {
    std::env::var_os("WICKED_SKILLS_SNAPSHOT")
        .map(std::path::PathBuf::from)
        .expect("setup() published the fixture snapshot once")
}

/// Every `SkillsSnapshotHanded` the run emitted so far, as `(path, gen, root)`.
fn handed_generations(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    run_id: &str,
) -> Vec<(String, Option<String>, String)> {
    events
        .try_iter()
        .filter_map(|e| match e {
            CoreEvent::SkillsSnapshotHanded {
                session,
                path,
                gen,
                root,
                ..
            } if session == run_id => Some((path, gen, root)),
            _ => None,
        })
        .collect()
}

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
        retired: false,
    }
}

fn is_terminal(s: SessionStatus) -> bool {
    matches!(
        s,
        SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
    )
}

/// Upper bound on how long a governed run may take before the test gives up.
///
/// Deliberately far above the ~2s a run actually takes. Every wait below returns the instant its
/// condition holds and fails fast on a terminal-but-wrong outcome, so a generous bound costs nothing
/// on an idle host and only ever spends time on a run that is genuinely stuck. An 8s bound was
/// reachable by scheduling noise alone — on a host under load these tests failed roughly one run in
/// three, on unmodified `main`, which is a test reporting a defect that isn't there (FINDING-028).
/// (M6) 180s: the preset's plan is PA-scoped and floor-filled, about eleven units where the drop-in
/// def had five, and a loaded host was measured at ~10s per unit.
const RUN_DEADLINE: Duration = Duration::from_secs(180);

/// Waits for `run_id` to reach `want`. `Err` says which way it went wrong — a timeout and a run that
/// terminated in the wrong status are different defects and must not read the same in the output.
fn wait_status(core: &Core, run_id: &str, want: SessionStatus) -> Result<(), String> {
    let deadline = Instant::now() + RUN_DEADLINE;
    let mut last = None;
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == run_id) {
                if v.session.status == want {
                    return Ok(());
                }
                // Fail fast: a terminal status that isn't the one we want will never change.
                if is_terminal(v.session.status) {
                    return Err(format!(
                        "run {run_id} terminated as {:?}, wanted {want:?}",
                        v.session.status
                    ));
                }
                let units: Vec<String> = v
                    .units
                    .iter()
                    .map(|u| {
                        format!(
                            "{}={:?}",
                            u.id.rsplit(':').next().unwrap_or_default(),
                            u.status
                        )
                    })
                    .collect();
                last = Some((v.session.status, units));
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    Err(format!(
        "run {run_id} did not reach {want:?} within {}s — it was still {} when the wait gave up, so \
         this is a timeout, not a wrong outcome",
        RUN_DEADLINE.as_secs(),
        last.map_or_else(
            || "absent from sessions_detail".to_string(),
            |(s, units)| format!("{s:?} (units: {})", units.join(", "))
        )
    ))
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
            .set_node_semantics(
                &n.symbol,
                None,
                Some("REQ-1"),
                Some(&ValidationClaim::new(true, "test-fixture").unwrap()),
            )
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

/// The domain-extraction runner: keys on the unit's PHASE id (DES-TEAMING-002 M6: the preset's plan
/// is PA-scoped and floor-filled, so ords move). Every other phase emits benign output; `coverage`
/// shells the REAL `wicked-core coverage` (recompute FROM the seeded store → the pinned coverage
/// validator greps `coverage-report.json` in the worktree); `domain-graph` shells the REAL
/// `wicked-core domain-graph` → writes `requirements_graph.json`. `out_override` lets a test inject a
/// policy-tripping output for one phase.
struct DomainExtractionRunner {
    db: String,
    out_override: Option<(&'static str, String)>,
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
                output: "PASS\nthe work meets the criterion\nPASS".into(),
                status: StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            };
        }
        let phase = i.unit.id.rsplit(':').next().unwrap_or_default();
        let workdir = i.workdir.clone();
        let mut output = format!("{phase} done");
        if phase == "pa-scope" {
            // The PA's answer. This rig has no bus (an UN-TEAMED run), so a declared scope is never
            // diff-corrected and fails closed at 100: the plan pauses for approval (X1 r2 M1).
            output.push_str(
                "\nSCOPE {\"touch\":[\"coverage-report.json\",\"requirements_graph.json\"]}",
            );
        }
        if let Some((p, ref text)) = self.out_override {
            if p == phase {
                output = text.clone();
            }
        }
        // DES-L1 PR-1A (D-9): the `coverage` phase is the def's Evaluator agent unit — its output
        // must answer the verdict contract or the fold parks the run at the escalation gate before
        // the domain-graph phase; the pinned coverage validator stays the real coverage check.
        if i.unit.role == wicked_core::PhaseRole::Evaluator && i.unit.tool_cmd.is_none() {
            output.push_str("\nVERDICT: PASS");
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
            if phase == "coverage" {
                let out = wd.join("coverage-report.json");
                shell(
                    &["coverage", "--db", &self.db, "--out", out.to_str().unwrap()],
                    "coverage",
                );
            } else if phase == "domain-graph" {
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
            tools: Vec::new(),
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
        // core#237: the domain-graph Tool phase runs `wicked-core domain-graph`, which `run_tool_cmd`
        // resolves via `resolve_wicked_core_exe` ($WICKED_CORE_EXE → current_exe → PATH → bare). Under
        // `cargo test` current_exe is THIS test harness — spawning it as `wicked-core` would re-exec
        // the suite (a fork bomb). Pin $WICKED_CORE_EXE to the real binary Cargo built so the Tool
        // (and the coverage recompute, same resolver) invoke the actual engine.
        std::env::set_var("WICKED_CORE_EXE", BIN);
        // Repo-graph root (code_graph.rs ADR, core#406): a fresh repo's code_graph_db resolves into
        // `$WICKED_ESTATE_REPO_GRAPH_ROOT` when set (else `<state home>/repo-graphs` — the parent of
        // the store the Core below is spawned on, itself a scratch dir). Pin the override too, so a
        // concurrent env test cannot move the root mid-run — the same hermetic override crew's
        // project graphs use.
        std::env::set_var(
            "WICKED_ESTATE_REPO_GRAPH_ROOT",
            std::env::temp_dir().join(format!("wicked-core-e2e-estate-{}", std::process::id())),
        );
        // core#396: the workflow's units name skills, and the plan-wide admission runs before the
        // FIRST unit (the domain-graph Tool phase included). Publish a fixture generation exactly as
        // crew lays one out and hand it to the engine — never the developer's live garden cache,
        // which a hermetic CI runner does not have. Canonical base: the loader refuses an ancestor
        // symlink, and the OS temp dir is one on macOS.
        let skills_base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("wicked-core-e2e-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&skills_base);
        let snapshot = skills_fixture::publish_fixture_snapshot(
            &skills_base,
            "000001",
            DOMAIN_EXTRACTION_SKILLS,
        );
        std::env::set_var("WICKED_SKILLS_SNAPSHOT", &snapshot);
        std::env::remove_var("WICKED_WORKER_INHERIT_OPERATOR_CONFIG");
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
    // core#237: domain-graph is now a real `wicked-core domain-graph --db {code_graph_db}` Tool and
    // the coverage gate recomputes from WICKED_COVERAGE_DB — BOTH read the REPO's OWN code graph, not
    // the engine store. Seed the repo's code_graph_db (the exact path the run resolves) so the real
    // executor path finds the fixture; the engine-store seed above still serves the mock's ord-4 shell.
    std::fs::create_dir_all(std::path::Path::new(&entry.code_graph_db).parent().unwrap()).unwrap();
    seed(&entry.code_graph_db, accounted);
    (core, entry.id, db, repo)
}

fn launch(core: &Core, run_id: &str, repo_ref: &str) {
    core.launch_run(LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "extract the domain model".into(),
        // Two seats: a team run never grades on its creator seat (§11.3 "Evaluator seat").
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Shared,
        session_id: run_id.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: Some(repo_ref.into()),
        workflow: Some("domain-extraction".into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        plan: None,
        deliver_step: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        primary: None,
        // (core#850) One seat: the review rides the creator's seat by the explicit opt-in.
        reduced_assurance: true,
        deliverables: Vec::new(),
    })
    .expect("launch domain-extraction");
}

/// Derive the run's worktree from the repo root `setup` returns (not by re-deriving the temp-path
/// scheme — so a change to the repo naming can't silently break this).
/// (M6) Drive a preset run until `done` holds, approving every human gate it pauses at first. The
/// first pause is the plan-approval gate (a creator plan the PA scopes, which on this un-teamed rig
/// scores 100 and pauses even in auto mode); a floor-added phase may pause too. Returns the phases
/// whose gates were approved (the unit before the cursor: `unit_ix` is the NEXT unit to execute),
/// for the assertion messages.
fn drive_until(
    core: &Core,
    run_id: &str,
    what: &str,
    done: impl Fn(&SessionView) -> bool,
) -> Vec<String> {
    let mut approved: Vec<String> = Vec::new();
    let deadline = Instant::now() + RUN_DEADLINE;
    let mut last = String::new();
    while Instant::now() < deadline {
        let Some(view) = core
            .sessions_detail()
            .ok()
            .and_then(|vs| vs.into_iter().find(|v| v.session.id == run_id))
        else {
            std::thread::sleep(Duration::from_millis(15));
            continue;
        };
        if done(&view) {
            return approved;
        }
        assert!(
            !is_terminal(view.session.status),
            "run {run_id} ended {:?} before {what} (gates approved: {approved:?})",
            view.session.status
        );
        let mut units = view.units.clone();
        units.sort_by_key(|u| u.ord);
        last = format!(
            "{:?} at ix {} ({})",
            view.session.status,
            view.session.unit_ix,
            units
                .iter()
                .map(|u| format!(
                    "{}={:?}",
                    u.id.rsplit(':').next().unwrap_or_default(),
                    u.status
                ))
                .collect::<Vec<_>>()
                .join(", ")
        );
        if view.session.status == SessionStatus::AwaitingHuman {
            let gated = view
                .session
                .unit_ix
                .checked_sub(1)
                .and_then(|i| units.get(i))
                .map(|u| u.id.rsplit(':').next().unwrap_or_default().to_string())
                .unwrap_or_default();
            assert!(
                approved.len() < 8,
                "run {run_id} kept pausing before {what}: {approved:?}, now at `{gated}`"
            );
            core.confirm_gate(
                run_id,
                HumanDecision::Approve {
                    amend: None,
                    amend_scope: Default::default(),
                },
            )
            .unwrap_or_else(|e| panic!("approve the `{gated}` gate: {e}"));
            approved.push(gated);
            // Let the approve land before reading the run again.
            let ix = view.session.unit_ix;
            let settle = Instant::now() + Duration::from_secs(30);
            while Instant::now() < settle {
                let moved = core
                    .sessions_detail()
                    .ok()
                    .and_then(|vs| vs.into_iter().find(|v| v.session.id == run_id))
                    .is_some_and(|v| {
                        v.session.status != SessionStatus::AwaitingHuman || v.session.unit_ix != ix
                    });
                if moved {
                    break;
                }
                std::thread::sleep(Duration::from_millis(15));
            }
            continue;
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    panic!(
        "run {run_id} never reached {what} within {}s; last: {last}; gates approved: {approved:?}",
        RUN_DEADLINE.as_secs()
    );
}

/// The run is paused at the human gate on `phase` (the unit before the cursor).
fn paused_at(view: &SessionView, phase: &str) -> bool {
    if view.session.status != SessionStatus::AwaitingHuman {
        return false;
    }
    let mut units = view.units.clone();
    units.sort_by_key(|u| u.ord);
    view.session
        .unit_ix
        .checked_sub(1)
        .and_then(|i| units.get(i))
        .is_some_and(|u| u.id.rsplit(':').next() == Some(phase))
}

/// `phase`'s unit has been rejected (a denied gate on it).
fn rejected(view: &SessionView, phase: &str) -> bool {
    view.units
        .iter()
        .any(|u| u.id.rsplit(':').next() == Some(phase) && u.status == UnitStatus::Rejected)
}

fn worktree(repo_root: &std::path::Path, run_id: &str) -> std::path::PathBuf {
    // crew#276: run worktrees live under the non-dotted root now.
    repo_root.join("wicked-worktrees").join(run_id)
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
    let events = core.subscribe();
    launch(&core, "run-happy", &repo_id);
    // The domain-graph phase carries a human-confirm gate → the run parks awaiting a human there.
    drive_until(&core, "run-happy", "the domain-graph gate", |v| {
        paused_at(v, "domain-graph")
    });
    // core#396: the run was admitted against the fixture generation and against nothing else —
    // the domain-graph Tool unit's plan-wide admission reports the verified generation it judged
    // the plan by (`path: "tool_cmd"`), the way a worker handoff would.
    let snapshot = skills_snapshot_root();
    let handed = handed_generations(&events, "run-happy");
    assert!(
        !handed.is_empty()
            && handed.iter().all(|(path, gen, root)| {
                path == "tool_cmd"
                    && gen.as_deref() == Some("000001")
                    && skills_fixture::names_generation(root, &snapshot)
            }),
        "the run's skills were admitted against the fixture generation 000001 at {}: {handed:?}",
        snapshot.display()
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
    core.confirm_gate(
        "run-happy",
        HumanDecision::Approve {
            amend: None,
            amend_scope: Default::default(),
        },
    )
    .expect("approve the domain-graph gate");
    wait_status(&core, "run-happy", SessionStatus::Completed)
        .expect("approving the final gate completes the governed run");
}

// --- TEST 2: a policy violation in a phase's output denies the phase ---

#[test]
fn a_policy_violation_denies_a_phase_and_halts_the_run() {
    let db = db_in("deny");
    // Trip a deny on the extractor phase (`extract`) via a token in its output.
    let (core, repo_id, dbp, repo) = setup(
        "deny",
        DomainExtractionRunner {
            db,
            out_override: Some(("extract", "emitting LEAKTOKEN in the output".into())),
        },
        true,
    );
    {
        let mut store = open_store(Some(&dbp)).unwrap();
        register_policy(&mut store, &deny_policy("extract", "LEAKTOKEN")).unwrap();
    }
    launch(&core, "run-deny", &repo_id);
    // A policy violation in the extractor phase's output denies it → the run pauses at the
    // escalation gate on that phase (core#464).
    drive_until(&core, "run-deny", "the extract deny", |v| {
        rejected(v, "extract")
    });
    // Attribute the failure to the EXTRACTOR phase specifically — not an unrelated gate — so the test
    // proves the policy-over-output deny, not just "some failure".
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "run-deny").unwrap();
    let extract = v.units.iter().find(|u| u.id == "run-deny:extract").unwrap();
    assert_eq!(
        extract.status,
        UnitStatus::Rejected,
        "the extractor phase is the denied unit"
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
                retired: false,
                ..Default::default()
            },
        )
        .unwrap();
    }
    let events = core.subscribe();
    launch(&core, "run-recall", &repo_id);
    drive_until(&core, "run-recall", "the domain-graph gate", |v| {
        paused_at(v, "domain-graph")
    });
    // core#396: admitted against the fixture generation, and only it (see TEST 1).
    let snapshot = skills_snapshot_root();
    let handed = handed_generations(&events, "run-recall");
    assert!(
        !handed.is_empty()
            && handed.iter().all(|(path, gen, root)| {
                path == "tool_cmd"
                    && gen.as_deref() == Some("000001")
                    && skills_fixture::names_generation(root, &snapshot)
            }),
        "admitted against generation 000001 at {}: {handed:?}",
        snapshot.display()
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
    // The coverage validator's deny escalates to the human gate on `coverage`; approve every gate
    // before it (the plan's, and any floor phase's).
    let approved = drive_until(&core, "run-hole", "the coverage deny", |v| {
        rejected(v, "coverage")
    });

    // Poll until the coverage phase is REJECTED — the deterministic coverage
    // validator denied the sub-1.0 report. A not-pass verdict on the `human_confirm_if verdict_not_pass`
    // coverage gate escalates rather than hard-failing, so assert on the UNIT, not the session status.
    let deadline = Instant::now() + RUN_DEADLINE;
    let mut rejected = false;
    let mut last = None;
    while Instant::now() < deadline {
        if let Ok(views) = core.sessions_detail() {
            if let Some(v) = views.iter().find(|v| v.session.id == "run-hole") {
                let coverage = v.units.iter().find(|u| u.id == "run-hole:coverage");
                if coverage.map(|u| u.status) == Some(UnitStatus::Rejected) {
                    rejected = true;
                    break;
                }
                last = Some((v.session.status, coverage.map(|u| u.status)));
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    assert!(
        rejected,
        "the pinned coverage validator DENIES a sub-1.0 coverage report in a run (the gate has teeth; \
         the agent-PASS shim does not rescue a hole) — the coverage unit was never rejected within {}s; \
         last seen: {last:?}; gates approved first: {approved:?}",
        RUN_DEADLINE.as_secs()
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

// ── Test-harness hygiene (core#311) — not a test ─────────────────────────────────────────────
/// Arm the hermetic emit spool BEFORE main (pre-main is single-threaded, so no test thread can
/// race it): engine paths under test fire coarse fire-and-forget `wicked.*` emissions, and with
/// no shared store configured those spool — which must land in a per-process temp file, never in
/// the operator's real `~/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue. Every
/// binary in this suite carries this block; `harness_hygiene.rs` fails the suite if one is missing.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only sets one process env var
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}
