//! (DES-APPS-001 K0) The Apps presets' rules, each pinned by a test that fails without them.

use super::*;
use crate::assurance::{self as a, QeOverride, RunAssurance};
use crate::domain::{
    AgentSession, HumanConfirm, QeReview, SessionStatus, UnitDenial, UnitStatus, WorkUnit,
};
use crate::mcp_gate::{McpAnnotations, McpCall, McpMode, McpToken, McpUnit};
use crate::scope::EntityMode;
use crate::workflow::{GateSpec, PhaseExecutor, PhaseRole, PriorUnitOutput, StepInput};
use wicked_apps_core::{open_store, GraphStore, ToNode};
use wicked_council::types::{Category, Confidence, InputMode};

fn cli(key: &str) -> crate::AgenticCli {
    crate::AgenticCli {
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

fn seeded() -> wicked_apps_core::SqliteStore {
    let mut store = open_store(Some(":memory:")).unwrap();
    crate::preset::seed_builtins(&mut store, 0).unwrap();
    store
}

/// A launch as admission reads it: the container floor, governed, a broker URL, a repo and an
/// evidence root, two usable seats, no word.
fn facts<'a>(
    workflow: &'a str,
    clis: &'a [crate::AgenticCli],
    qe: &'a QeOverride,
) -> LaunchFacts<'a> {
    LaunchFacts {
        run_id: "r-admit",
        workflow: Some(workflow),
        project_id: None,
        repo: true,
        evidence_root: true,
        governed: true,
        crew_url: true,
        plan: false,
        reduced_assurance: false,
        power: false,
        deliver_step: false,
        qe,
        clis,
        exclude_seats: &[],
        floor: Some("container"),
    }
}

fn session(id: &str, assurance: RunAssurance) -> AgentSession {
    AgentSession {
        intent_amendments: Vec::new(),
        id: id.into(),
        workflow_id: format!("wf-{id}"),
        problem: "p".into(),
        entity_mode: EntityMode::Shared,
        collection_scope: None,
        clis: vec!["claude".into(), "codex".into()],
        status: SessionStatus::Executing,
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        unit_ix: 0,
        attempt: 0,
        workdir: None,
        repo_ref: None,
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
        project_id: None,
        archived_at: None,
        archive_note: None,
        verified_tree: None,
        run_branch: None,
        base_commit: None,
        finished_at: None,
        seat_failures: Vec::new(),
        benched_seats: Vec::new(),
        team: None,
        team_plan: None,
        exclude_seats: Vec::new(),
        evidence_root: None,
        assurance,
        power: None,
        run_context: None,
    }
}

/// The contract an Apps launch of `preset` is built with (instruments from the catalog, no word).
fn apps_contract(preset: &str) -> RunAssurance {
    let declared = crate::catalog::builtin_preset_instruments(preset).unwrap();
    contract(
        RunAssurance::new(Some(&declared), false),
        Some(AppsContract {
            preset: preset.to_string(),
        }),
        &QeOverride::Auto,
    )
    .unwrap()
}

fn def_of(store: &dyn wicked_apps_core::GraphRead, name: &str) -> crate::workflow::WorkflowDef {
    crate::pipeline::resolve_workflow_def(store, None, Some(name), None)
        .unwrap()
        .unwrap_or_else(|| panic!("`{name}` resolves"))
}

fn ids(def: &crate::workflow::WorkflowDef) -> Vec<&str> {
    def.phases.iter().map(|p| p.id.as_str()).collect()
}

fn tool_cmd(def: &crate::workflow::WorkflowDef, id: &str) -> Vec<String> {
    match &def.phases.iter().find(|p| p.id == id).unwrap().executor {
        PhaseExecutor::Tool { cmd } => cmd.clone(),
        other => panic!("{id} is not a Tool: {other:?}"),
    }
}

// ─── The presets ────────────────────────────────────────────────────────────────────────────────

/// §9.2-§9.3: both presets are the designed steps, composed straight over the catalog with their
/// own instruments, and never enter the plan gate (no PA scope, floor fill or re-plan could drop
/// the review or add a step).
#[test]
fn the_two_presets_are_fixed_plans_of_the_designed_steps() {
    let store = seeded();
    let turn = def_of(&store, APPS_TURN);
    assert_eq!(turn.id, APPS_TURN);
    assert_eq!(ids(&turn), [BUILD_STEP, CHECK_STEP, REVIEW_STEP]);
    let build = &turn.phases[0];
    assert_eq!(build.role, PhaseRole::Creator);
    assert_eq!(build.skill_ref.as_deref(), Some(APPS_SKILL));
    assert_eq!(tool_cmd(&turn, CHECK_STEP), [CREW_BIN, "apps-check"]);
    let review = &turn.phases[2];
    assert_eq!(review.role, PhaseRole::Evaluator);
    assert_eq!(review.skill_ref.as_deref(), Some(APPS_SKILL));
    assert_eq!(review.depends_on, [CHECK_STEP]);
    assert_eq!(
        turn.required_instruments.as_deref(),
        Some(&[a::DISTINCT_EVALUATOR.to_string()][..])
    );

    let publish = def_of(&store, APPS_PUBLISH);
    assert_eq!(ids(&publish), [CHECK_STEP, QE_STEP, PRODUCE_STEP]);
    assert_eq!(
        tool_cmd(&publish, CHECK_STEP),
        [CREW_BIN, "apps-check", "--publish"]
    );
    assert_eq!(tool_cmd(&publish, PRODUCE_STEP), [CREW_BIN, "apps-produce"]);
    assert_eq!(publish.phases[2].gate, GateSpec::ConsentBefore);
    assert_eq!(publish.phases[1].role, PhaseRole::Evaluator);
    assert!(publish
        .required_instruments
        .as_ref()
        .unwrap()
        .contains(&a::QE_ACCEPTANCE.to_string()));

    for name in [APPS_TURN, APPS_PUBLISH] {
        assert!(
            crate::plan_gate::launch_plan(&store, None, Some(name), None, &[])
                .unwrap()
                .is_none(),
            "`{name}` never enters the plan gate"
        );
    }
    // A non-Apps preset still does.
    assert!(
        crate::plan_gate::launch_plan(&store, None, Some("feature"), None, &[])
            .unwrap()
            .is_some()
    );
}

/// §9.3: a publish builds nothing, yet its `qe` step is the run's QE unit (ledger root, isolated
/// reviewer, attributed verdict); its produce step asks before it runs.
#[test]
fn the_publish_qe_step_is_the_runs_qe_unit() {
    let store = seeded();
    let units = crate::plan::plan_from_def(&def_of(&store, APPS_PUBLISH), "publish", "r-pub");
    let qe = units
        .iter()
        .find(|u| u.phase_id() == Some(QE_STEP))
        .unwrap();
    assert!(crate::qe_acceptance::is_qe_unit(qe), "{qe:?}");
    assert!(
        qe.validator.is_none(),
        "no diff floor: a publish changes no tree"
    );
    let produce = units
        .iter()
        .find(|u| u.phase_id() == Some(PRODUCE_STEP))
        .unwrap();
    assert_eq!(produce.gate, GateSpec::ConsentBefore);
    // Only the publish's: the same step id in another def is not a QE unit.
    let mut other = def_of(&store, APPS_PUBLISH);
    other.id = "not-apps".into();
    let units = crate::plan::plan_from_def(&other, "publish", "r-other");
    assert!(!units.iter().any(crate::qe_acceptance::is_qe_unit));
}

#[test]
fn the_apps_names_are_reserved_in_every_scope() {
    let mut store = seeded();
    for name in [APPS_TURN, APPS_PUBLISH] {
        for project in [None, Some("p1")] {
            let err = crate::preset::put_preset(
                &mut store,
                crate::preset::PresetSpec {
                    name: name.into(),
                    project_id: project.map(str::to_string),
                    steps: crate::catalog::builtin_preset(name).unwrap(),
                    created_by: "api".into(),
                },
                1,
            )
            .unwrap_err()
            .to_string();
            assert!(err.starts_with("preset_reserved_name:"), "{err}");
        }
    }
}

/// A stored row under an Apps name that is not the built-in (written before the names were
/// reserved) never launches as saved.
#[test]
fn a_saved_row_under_an_apps_name_refuses_the_launch() {
    let mut store = seeded();
    let row = crate::preset::Preset {
        name: APPS_TURN.into(),
        scope: "project:p1".into(),
        steps: vec![crate::plan::PlanStep {
            catalog: "build".into(),
            id: "build".into(),
            ..Default::default()
        }],
        created_by: "api".into(),
        updated_at: 1,
        deleted_at: None,
        required_instruments: None,
        title: None,
        summary: None,
        use_when: Vec::new(),
    };
    crate::domain::put_node(&mut store, row.to_node()).unwrap();
    let clis = [cli("claude"), cli("codex")];
    let qe = QeOverride::Auto;
    let f = LaunchFacts {
        project_id: Some("p1"),
        ..facts(APPS_TURN, &clis, &qe)
    };
    let err = admit(&store, &f).unwrap_err().to_string();
    assert!(err.contains("reserved name"), "{err}");
    assert!(contract_for(&store, Some("p1"), Some(APPS_TURN))
        .unwrap()
        .is_none());
    // The global built-in still admits.
    assert!(admit(&store, &facts(APPS_TURN, &clis, &qe))
        .unwrap()
        .is_some());
}

// ─── Launch admission ───────────────────────────────────────────────────────────────────────────

/// Acceptance: either preset on the host floor (or any floor but the container one) is refused.
#[test]
fn either_preset_off_the_container_floor_is_refused() {
    let store = seeded();
    let clis = [cli("claude"), cli("codex")];
    let qe = QeOverride::Auto;
    for name in [APPS_TURN, APPS_PUBLISH] {
        for floor in [Some("host"), None, Some(""), Some("seatbelt")] {
            let f = LaunchFacts {
                floor,
                ..facts(name, &clis, &qe)
            };
            let err = admit(&store, &f).unwrap_err().to_string();
            assert!(err.contains("container floor"), "{name} {floor:?}: {err}");
        }
        let ok = admit(&store, &facts(name, &clis, &qe)).unwrap().unwrap();
        assert_eq!(ok.preset, name);
        // …and at dispatch, from the session's contract.
        assert!(floor_refusal(name, Some("host")).is_some());
        assert!(floor_refusal(name, Some("container")).is_none());
    }
    // A launch naming no Apps preset is not judged here.
    let f = LaunchFacts {
        floor: Some("host"),
        ..facts("feature", &clis, &qe)
    };
    assert!(admit(&store, &f).unwrap().is_none());
}

#[test]
fn reduced_assurance_power_a_deliver_step_and_a_plan_are_refused() {
    let store = seeded();
    let clis = [cli("claude"), cli("codex")];
    let qe = QeOverride::Auto;
    for name in [APPS_TURN, APPS_PUBLISH] {
        let base = || facts(name, &clis, &qe);
        for (f, want) in [
            (
                LaunchFacts {
                    reduced_assurance: true,
                    ..base()
                },
                "reduced-assurance",
            ),
            (
                LaunchFacts {
                    power: true,
                    ..base()
                },
                "power mode",
            ),
            (
                LaunchFacts {
                    deliver_step: true,
                    ..base()
                },
                "deliver step",
            ),
            (
                LaunchFacts {
                    plan: true,
                    ..base()
                },
                "no plan",
            ),
        ] {
            let err = admit(&store, &f).unwrap_err().to_string();
            assert!(err.contains(want), "{name}: {err}");
        }
    }
}

/// Acceptance (launch half): a QE skip on `apps-publish` is refused; every other word lands on
/// the preset's forced decision, which no score re-decides.
#[test]
fn a_qe_skip_on_publish_is_refused_at_launch_and_qe_is_forced() {
    let store = seeded();
    let clis = [cli("claude"), cli("codex")];
    let skip = QeOverride::Skip("demo tomorrow".into());
    let err = admit(&store, &facts(APPS_PUBLISH, &clis, &skip))
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot be skipped"), "{err}");
    let declared = crate::catalog::builtin_preset_instruments(APPS_PUBLISH).unwrap();
    let publish = || {
        Some(AppsContract {
            preset: APPS_PUBLISH.into(),
        })
    };
    assert!(contract(RunAssurance::new(Some(&declared), false), publish(), &skip).is_err());
    for word in [
        QeOverride::Auto,
        QeOverride::Force,
        QeOverride::Standing("QE off on this daemon".into()),
    ] {
        let run = contract(RunAssurance::new(Some(&declared), false), publish(), &word).unwrap();
        let qe = run.qe.clone().unwrap();
        assert_eq!(
            (qe.status.as_str(), qe.basis.as_str()),
            (a::QE_REQUIRED, a::QE_BASIS_PRESET),
            "{word:?}"
        );
        assert_eq!(qe.reason, QE_FORCED_REASON);
        assert!(run.enforces(a::QE_ACCEPTANCE));
        assert_eq!(run.apps.as_ref().unwrap().preset, APPS_PUBLISH);
        // No score re-decides it at the QE unit's dispatch (a score that would waive included).
        let s = session("r-forced", run);
        let mut u = WorkUnit::pending("r-forced:qe", "r-forced", 2, "qe");
        u.repo_checks_floor = true;
        u.role = PhaseRole::Evaluator;
        assert!(matches!(
            crate::qe_acceptance::on_dispatch(&s, &u, None, None),
            crate::qe_acceptance::Dispatched::Unchanged
        ));
    }
    // A turn takes no QE word but the host's standing one, which has nothing to skip there.
    let turn = || {
        Some(AppsContract {
            preset: APPS_TURN.into(),
        })
    };
    for word in [QeOverride::Force, QeOverride::Skip("x".into())] {
        assert!(admit(&store, &facts(APPS_TURN, &clis, &word)).is_err());
        assert!(contract(RunAssurance::default(), turn(), &word).is_err());
    }
    let run = contract(
        RunAssurance::default(),
        turn(),
        &QeOverride::Standing("off".into()),
    )
    .unwrap();
    assert!(run.qe.is_none() && run.enforces(a::DISTINCT_EVALUATOR));
}

/// Acceptance: Publish with every seat a builder is refused, naming why; the exclusion is a bench
/// that leads the bench and never lifts.
#[test]
fn publish_with_every_seat_a_builder_is_refused() {
    let store = seeded();
    let clis = [cli("claude"), cli("codex")];
    let qe = QeOverride::Auto;
    let builders = ["claude".to_string(), "codex".to_string()];
    let f = LaunchFacts {
        exclude_seats: &builders,
        ..facts(APPS_PUBLISH, &clis, &qe)
    };
    let err = admit(&store, &f).unwrap_err();
    let seat = err
        .downcast_ref::<crate::actor::NoEligibleSeat>()
        .expect("the typed NoEligibleSeat refusal");
    assert!(seat
        .benched_seats
        .iter()
        .all(|b| b.source == EXCLUSION_BENCH_SOURCE));
    assert!(format!("{err:#}").contains("built a turn since the last publish"));
    assert_eq!(seat.run_id, "r-admit", "the refusal names the launch's run");
    // One seat that built nothing is not enough: the QE executor and its isolated reviewer are
    // two seats (pi r1: one seat would review its own evidence).
    let one = ["claude".to_string()];
    let f = LaunchFacts {
        exclude_seats: &one,
        ..facts(APPS_PUBLISH, &clis, &qe)
    };
    let err = admit(&store, &f).unwrap_err().to_string();
    assert!(err.contains("1 such seat"), "{err}");
    let three = [cli("claude"), cli("codex"), cli("pi")];
    let f = LaunchFacts {
        exclude_seats: &one,
        ..facts(APPS_PUBLISH, &three, &qe)
    };
    assert!(admit(&store, &f).unwrap().is_some());
    // An empty roster is refused too (pi r1).
    assert!(admit(&store, &facts(APPS_PUBLISH, &[], &qe)).is_err());
    // The bench leads, and a later bench of another cause (a quota bench, which can lift) never
    // replaces it.
    let apps = AppsContract {
        preset: APPS_PUBLISH.into(),
    };
    let quota = BenchedSeat {
        cli: "claude".into(),
        reason: "quota_exhausted".into(),
        source: "worker".into(),
        governed_only: false,
        benched_until: None,
        strikes: 1,
        replaced: None,
    };
    let bench = with_exclusion_benches(Some(&apps), &one, &clis, vec![quota.clone()]);
    assert_eq!(bench.len(), 1);
    assert_eq!(bench[0].source, EXCLUSION_BENCH_SOURCE);
    assert_eq!(bench[0].benched_until, None);
    let mut later = bench.clone();
    crate::domain::bench_seat(&mut later, quota);
    assert_eq!(later, bench);
    // A turn's launch exclusion stays a judge exclusion only.
    let turn = AppsContract {
        preset: APPS_TURN.into(),
    };
    assert!(exclusion_benches(&turn, &one, &clis).is_empty());
    // A CLI key excludes every instance of that CLI, an instance key only itself (codex r1: the
    // QE reviewer's own exclusion rule).
    let instances = [cli("claude"), cli("claude#2"), cli("codex")];
    let keys = |ex: &[&str]| -> Vec<String> {
        let ex: Vec<String> = ex.iter().map(|e| e.to_string()).collect();
        exclusion_benches(&apps, &ex, &instances)
            .into_iter()
            .map(|b| b.cli)
            .collect()
    };
    assert_eq!(keys(&["claude"]), ["claude", "claude#2"]);
    assert_eq!(keys(&["claude#2"]), ["claude#2"]);
    let f = LaunchFacts {
        exclude_seats: &builders,
        ..facts(APPS_PUBLISH, &instances, &qe)
    };
    assert!(
        admit(&store, &f).is_err(),
        "every instance of an excluded CLI is benched"
    );
}

/// §9.2: every turn is reviewed by a distinct seat, so a roster with one usable seat is refused at
/// launch ("the person edits directly or adds a seat").
#[test]
fn a_turn_needs_a_second_usable_seat() {
    let store = seeded();
    let qe = QeOverride::Auto;
    let one = [cli("claude")];
    let err = admit(&store, &facts(APPS_TURN, &one, &qe))
        .unwrap_err()
        .to_string();
    assert!(err.contains("distinct from its builder"), "{err}");
    let mut signed_out = cli("codex");
    signed_out.health = Some(wicked_council::types::SeatHealth {
        usable: false,
        reason: Some("signed out".into()),
    });
    let two = [cli("claude"), signed_out];
    assert!(admit(&store, &facts(APPS_TURN, &two, &qe)).is_err());
    let two = [cli("claude"), cli("codex")];
    assert!(admit(&store, &facts(APPS_TURN, &two, &qe)).is_ok());
}

#[test]
fn crews_cli_resolves_through_its_env() {
    assert_eq!(
        crew_bin_from(Some("/opt/crew/bin/wicked-crew")),
        "/opt/crew/bin/wicked-crew"
    );
    assert_eq!(crew_bin_from(None), CREW_BIN);
    assert_eq!(crew_bin_from(Some("  ")), CREW_BIN);
}

/// codex r1: an Apps launch that could not hold its rules is refused at admission — no input
/// governance (no ban, no containment), no broker URL (no unit token for crew's route), no app
/// repository, or a publish with no evidence root (no place for its QE verdict) — with the typed
/// refusal the bus bridge drops instead of retrying.
#[test]
fn an_apps_launch_that_could_not_hold_its_rules_is_refused() {
    let store = seeded();
    let clis = [cli("claude"), cli("codex"), cli("pi")];
    let qe = QeOverride::Auto;
    for name in [APPS_TURN, APPS_PUBLISH] {
        let base = || facts(name, &clis, &qe);
        for (f, want) in [
            (
                LaunchFacts {
                    governed: false,
                    ..base()
                },
                "input governance",
            ),
            (
                LaunchFacts {
                    crew_url: false,
                    ..base()
                },
                "broker URL",
            ),
            (
                LaunchFacts {
                    repo: false,
                    ..base()
                },
                "registered repository",
            ),
        ] {
            let err = admit(&store, &f).unwrap_err();
            assert!(err.downcast_ref::<AppsLaunchRefused>().is_some(), "{err:#}");
            assert!(err.to_string().contains(want), "{name}: {err}");
        }
    }
    let f = LaunchFacts {
        evidence_root: false,
        ..facts(APPS_PUBLISH, &clis, &qe)
    };
    assert!(admit(&store, &f)
        .unwrap_err()
        .to_string()
        .contains("evidence root"));
    let f = LaunchFacts {
        evidence_root: false,
        ..facts(APPS_TURN, &clis, &qe)
    };
    assert!(admit(&store, &f).is_ok(), "a turn records no QE verdict");
    // A QE word refusal is typed the same way.
    let skip = QeOverride::Skip("x".into());
    let err = admit(&store, &facts(APPS_PUBLISH, &clis, &skip)).unwrap_err();
    assert!(err.downcast_ref::<AppsLaunchRefused>().is_some());
}

// ─── No write or destructive registry tool for any Apps unit ─────────────────────────────────────

/// The input a carrier hands unit `u` of a run whose contract names `apps`.
fn input_for(u: &WorkUnit, db_path: &str, apps: Option<&str>) -> StepInput {
    StepInput {
        run_id: u.session_id.clone(),
        unit_ix: (u.ord - 1) as usize,
        attempt: 0,
        unit: u.clone(),
        workflow_id: "wf".into(),
        entity_mode: EntityMode::Shared,
        workdir: Some(std::env::temp_dir()),
        governance: Some(crate::workflow::GovernanceContext {
            db_path: db_path.to_string(),
            code_graph_db: None,
            extra_write_roots: Vec::new(),
            extra_read_roots: Vec::new(),
            project_id: None,
            repo_id: None,
            human_confirm: HumanConfirm::None,
            apps: apps.map(str::to_string),
        }),
        prior_outputs: Vec::new(),
        elicitation_epoch: 0,
        process_gen: None,
        launch_seq: 0,
        required_skills: Vec::new(),
    }
}

/// An already registered REST source's read tool (its first use approved): what any unit of any
/// other run may call and list.
fn rest_read() -> McpCall {
    McpCall {
        server: "support-api".into(),
        tool: "get_tickets".into(),
        args: serde_json::json!({}),
        annotations: Some(McpAnnotations {
            read_only_hint: Some(true),
            ..Default::default()
        }),
        class_override: None,
        registered: true,
        kind: "rest".into(),
        carrier: "shim".into(),
    }
}

fn policy_db(name: &str) -> (std::path::PathBuf, String) {
    let tid = format!("{:?}", std::thread::current().id()).replace(['(', ')'], "");
    let base =
        std::env::temp_dir().join(format!("wicked-apps-{name}-{}-{tid}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let db = base.join("policy.db").to_string_lossy().into_owned();
    let mut store = open_store(Some(&db)).unwrap();
    crate::mcp_gate::seed_mcp_defaults(&mut store).unwrap();
    let mut first_use = crate::mcp_gate::mcp_default_rules()
        .unwrap()
        .into_iter()
        .find(|r| r.id == crate::mcp_gate::FIRST_USE_LEDGER)
        .unwrap();
    first_use.excludes.push("mcp:support-api".into());
    wicked_governance::register_rule(&mut store, &first_use).unwrap();
    (base, db)
}

/// Every unit of both presets, and the isolated QE reviewer's own input.
fn every_unit(store: &dyn wicked_apps_core::GraphRead) -> Vec<(String, WorkUnit)> {
    let mut out = Vec::new();
    for name in [APPS_TURN, APPS_PUBLISH] {
        for u in crate::plan::plan_from_def(&def_of(store, name), "p", &format!("r-{name}")) {
            out.push((name.to_string(), u));
        }
    }
    out
}

fn rest_tool(tool: &str, read_only: Option<bool>, destructive: Option<bool>) -> McpCall {
    McpCall {
        tool: tool.into(),
        annotations: Some(McpAnnotations {
            read_only_hint: read_only,
            destructive_hint: destructive,
            ..Default::default()
        }),
        ..rest_read()
    }
}

/// A write tool (`readOnlyHint: false, destructiveHint: false`).
fn rest_write() -> McpCall {
    rest_tool("update_ticket", Some(false), Some(false))
}

/// A destructive tool (`destructiveHint` true).
fn rest_destructive() -> McpCall {
    rest_tool("delete_ticket", Some(false), Some(true))
}

/// A policy deny on one read tool (the gates an Apps read still passes, codex r1).
fn deny_read_rule(db: &str) {
    let mut store = open_store(Some(db)).unwrap();
    let rule: wicked_governance::ConformanceRule = serde_json::from_value(serde_json::json!({
        "id": "SEC-NO-SECRET-TICKETS", "rule_type": "policy",
        "statement": "never read the secret tickets", "severity": "critical",
        "confidence": 1.0, "steering_type": "security",
        "applies_to": ["mcp:support-api/get_secret_tickets"], "effect": "deny",
        "provenance": {"source": "ui", "ref": "test", "source_kinds": ["doc"]}
    }))
    .unwrap();
    rule.validate().unwrap();
    wicked_governance::register_rule(&mut store, &rule).unwrap();
}

/// Every write-class shape D-7 must refuse: explicit hints, the fail-closed defaults (no
/// annotations, empty annotations, `readOnlyHint: false` alone) and an operator's override.
fn every_write_shape() -> Vec<McpCall> {
    vec![
        rest_write(),
        rest_destructive(),
        McpCall {
            tool: "post_ticket".into(),
            annotations: None,
            ..rest_read()
        },
        McpCall {
            tool: "patch_ticket".into(),
            annotations: Some(McpAnnotations::default()),
            ..rest_read()
        },
        rest_tool("merge_tickets", Some(false), None),
        McpCall {
            tool: "get_tickets_and_close".into(),
            class_override: Some(crate::mcp_gate::McpClass::Write),
            ..rest_read()
        },
        McpCall {
            tool: "get_tickets_and_purge".into(),
            class_override: Some(crate::mcp_gate::McpClass::Destructive),
            ..rest_read()
        },
    ]
}

/// Acceptance (D-7 as DES-KNOWLEDGE-DATA-001 §9.3 narrows it): builder, reviewer and QE units (and
/// the QE reviewer turn, and the check Tool's token) of an Apps run calling or listing a `write` or
/// `destructive` tool — by its hints, by the fail-closed defaults, or by an operator's override,
/// registered or not — are refused, while the same unit calls and lists a `read` tool exactly as
/// the same unit off an Apps run does: the read still passes D-5, first use and policy (crew's data
/// gate then decides rows and visibility). Off an Apps run the same write is reachable for a
/// creator and D-1's for an evaluator, so D-7 is what refuses it.
#[test]
fn an_apps_unit_lists_and_calls_read_tools_and_never_a_write_or_destructive_one() {
    let presets = seeded();
    let (base, db) = policy_db("ban");
    deny_read_rule(&db);
    let store = open_store(Some(&db)).unwrap();
    let read = rest_read();
    // An operator's override to read over write hints is the class contract (D-4), not a bypass.
    let read_override = McpCall {
        tool: "refresh_ticket_cache".into(),
        class_override: Some(crate::mcp_gate::McpClass::Read),
        ..rest_write()
    };
    let writes = every_write_shape();
    // Reads the ordinary gates refuse or defer, Apps or not.
    let unregistered_read = McpCall {
        tool: "get_archived_tickets".into(),
        registered: false,
        ..rest_read()
    };
    let unapproved_read = McpCall {
        server: "other-api".into(),
        ..rest_read()
    };
    let denied_read = McpCall {
        tool: "get_secret_tickets".into(),
        ..rest_read()
    };
    let verdict =
        |unit: &McpUnit, c: &McpCall| crate::mcp_gate::evaluate(&store, unit, c, 1).unwrap().0;
    let mut checked = Vec::new();
    for (preset, u) in every_unit(&presets) {
        let mut inputs = vec![input_for(&u, &db, Some(&preset))];
        if crate::qe_acceptance::is_qe_unit(&u) {
            let seat = crate::domain::QeReviewerSeat {
                seat: "codex".into(),
                basis: "distinct".into(),
                same_cli: false,
                invocation: None,
            };
            inputs.push(crate::qe_acceptance::review::reviewer_input(
                &inputs[0], &seat, "/ev", "nonce",
            ));
        }
        for input in inputs {
            let at = format!("{preset} unit {:?}", u.phase_id());
            let unit = McpUnit::of(&input, &db, McpMode::Autonomous);
            let mut plain_input = input.clone();
            plain_input.governance.as_mut().unwrap().apps = None;
            let plain = McpUnit::of(&plain_input, &db, McpMode::Autonomous);

            assert_eq!(verdict(&unit, &read).decision, "allow", "{at} read");
            assert_eq!(
                verdict(&unit, &read_override).decision,
                "allow",
                "{at} read override"
            );
            // The ordinary gates hold for an Apps read exactly as they do off an Apps run.
            for (c, decision, rule) in [
                (
                    &unregistered_read,
                    "deny",
                    crate::mcp_gate::RULE_UNREGISTERED,
                ),
                (&unapproved_read, "ask", crate::mcp_gate::RULE_FIRST_USE),
                (&denied_read, "deny", "SEC-NO-SECRET-TICKETS"),
            ] {
                for who in [&unit, &plain] {
                    let v = verdict(who, c);
                    assert_eq!(
                        v.decision, decision,
                        "{at} {} (apps: {:?}): {v:?}",
                        c.tool, who.apps
                    );
                    assert_eq!(
                        v.rule_ids.first().map(String::as_str),
                        Some(rule),
                        "{at} {}",
                        c.tool
                    );
                }
            }
            for w in &writes {
                let v = verdict(&unit, w);
                assert_eq!(v.decision, "deny", "{at} {}", w.tool);
                assert_eq!(v.rule_ids, [RULE_NO_SOURCE_TOOL], "{at} {}", w.tool);
                // Unregistered or not, a write is D-7's first.
                let gone = McpCall {
                    registered: false,
                    ..w.clone()
                };
                assert_eq!(
                    verdict(&unit, &gone).rule_ids,
                    [RULE_NO_SOURCE_TOOL],
                    "{at} {} unregistered",
                    w.tool
                );
                // Off an Apps run the same write is the creator's to make, or D-1's to refuse.
                let v = verdict(&plain, w);
                if plain.role == PhaseRole::Evaluator
                    || plain.posture == crate::write_posture::WritePosture::ReadOnly
                {
                    assert_eq!(
                        v.rule_ids,
                        [crate::mcp_gate::RULE_PHASE_ROLE],
                        "{at} {} off Apps: {v:?}",
                        w.tool
                    );
                } else {
                    assert_ne!(v.decision, "deny", "{at} {} off Apps: {v:?}", w.tool);
                }
            }
            let token = McpToken::mint();
            let _b = token.bind(unit);
            let mut calls = vec![
                read.clone(),
                read_override.clone(),
                unregistered_read.clone(),
                unapproved_read.clone(),
                denied_read.clone(),
            ];
            calls.extend(writes.iter().cloned());
            let listed = crate::mcp_gate::list_mcp_tools(token.value(), &calls).unwrap();
            let subjects: Vec<(&str, &str)> = listed
                .tools
                .iter()
                .map(|t| (t.subject.as_str(), t.decision))
                .collect();
            assert_eq!(
                subjects,
                [
                    ("mcp:support-api/get_tickets", "allow"),
                    ("mcp:support-api/refresh_ticket_cache", "allow"),
                    ("mcp:other-api/get_tickets", "ask"),
                ],
                "{at} lists {listed:?}"
            );
            checked.push(format!("{preset}/{}", u.phase_id().unwrap_or("?")));
        }
    }
    // build, check, review; check, qe (+ its reviewer), produce.
    assert_eq!(checked.len(), 7, "{checked:?}");
    // The check Tool's own token (the route's credential) lists the read tool and no write.
    let (u_preset, check) = every_unit(&presets)
        .into_iter()
        .find(|(p, u)| p == APPS_TURN && u.phase_id() == Some(CHECK_STEP))
        .unwrap();
    let (token, _binding, env) = crate::mcp_gate::arm_tool_unit_channel_at(
        &input_for(&check, &db, Some(&u_preset)),
        Some("http://127.0.0.1:7700".into()),
    )
    .expect("armed");
    assert!(env
        .iter()
        .any(|(k, v)| k == crate::mcp_gate::TOKEN_ENV && v == token.value()));
    let mut calls = vec![read];
    calls.extend(writes);
    let listed = crate::mcp_gate::list_mcp_tools(token.value(), &calls).unwrap();
    assert_eq!(
        listed
            .tools
            .iter()
            .map(|t| t.subject.as_str())
            .collect::<Vec<_>>(),
        ["mcp:support-api/get_tickets"]
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// Acceptance (S5, as DES-KNOWLEDGE-DATA-001 §9.3 amends it): marker strings that a write tool (an
/// action on a fixture source) would hand back appear in no unit's context pack of an Apps run. The
/// engine's only path for a tool's answer into an Apps seat is a brokered call: the broker hands an
/// answer only on `allow`, and each unit's output becomes the next unit's prior output, as the actor
/// threads it. Under an Apps run no write is ever allowed, so neither the calling seat nor any later
/// context holds its answer. A READ tool's answer now reaches an Apps unit as it reaches any unit:
/// rows and visibility are crew's data gate (out of this test's scope: crew's
/// `tests/sources-knowledge-package.test.ts` covers a query-off source), and the controls below show
/// the harness would catch a marker. (That the real actor stamps the Apps marker on every
/// dispatched input, QE reviewer turn included, is `tests/apps_presets.rs`.)
#[test]
fn marker_strings_behind_a_write_tool_reach_no_apps_context_pack() {
    const MARKERS: [&str; 3] = [
        "MARKER-7f3c-ticket-subject",
        "MARKER-91ab-customer-email",
        "MARKER-c0de-upload-cell",
    ];
    let presets = seeded();
    let (base, db) = policy_db("markers");
    let store = open_store(Some(&db)).unwrap();
    // The fixture source's answer, whatever tool hands it back.
    let rows = format!(
        "{{\"items\":[{{\"subject\":\"{}\",\"email\":\"{}\",\"cell\":\"{}\"}}]}}",
        MARKERS[0], MARKERS[1], MARKERS[2]
    );
    // A seat lists its tools, then calls the source whether or not it was listed; the broker
    // answers only on `allow`.
    let broker = |unit: &McpUnit, call: &McpCall| -> Option<String> {
        let token = McpToken::mint();
        let _b = token.bind(unit.clone());
        let _listed =
            crate::mcp_gate::list_mcp_tools(token.value(), std::slice::from_ref(call)).unwrap();
        let (v, _) = crate::mcp_gate::evaluate(&store, unit, call, 1).unwrap();
        (v.decision == "allow").then(|| rows.clone())
    };
    let run = |apps: bool, call: &McpCall| -> Vec<String> {
        let mut packs = Vec::new();
        for name in [APPS_TURN, APPS_PUBLISH] {
            let mut prior: Vec<PriorUnitOutput> = Vec::new();
            let units =
                crate::plan::plan_from_def(&def_of(&presets, name), "chat message", "r-markers");
            for u in units.iter().filter(|u| u.tool_cmd.is_none()) {
                let mut input = input_for(u, &db, apps.then_some(name));
                input.prior_outputs = prior.clone();
                let mut inputs = vec![input.clone()];
                if crate::qe_acceptance::is_qe_unit(u) {
                    let seat = crate::domain::QeReviewerSeat {
                        seat: "codex".into(),
                        basis: "distinct".into(),
                        same_cli: false,
                        invocation: None,
                    };
                    inputs.push(crate::qe_acceptance::review::reviewer_input(
                        &input, &seat, "/ev", "nonce",
                    ));
                }
                for i in &inputs {
                    // The context pack: everything the engine hands the seat, and what the broker
                    // hands THIS seat when it calls (the QE reviewer turn's own call included, and
                    // a terminal unit's, which no later context would show).
                    let received = match broker(&McpUnit::of(i, &db, McpMode::Autonomous), call) {
                        Some(rows) => format!("read the source: {rows}"),
                        None => "refused".to_string(),
                    };
                    packs.push(format!(
                        "{}\n{}\n{}\n{:?}\n{received}",
                        i.unit.description,
                        i.prior_outputs
                            .iter()
                            .map(|p| format!("{}: {}", p.label, p.output))
                            .collect::<Vec<_>>()
                            .join("\n"),
                        i.run_id,
                        i.governance.as_ref().map(|g| g.apps.clone()),
                    ));
                }
                let unit = McpUnit::of(&input, &db, McpMode::Autonomous);
                let output = match broker(&unit, call) {
                    Some(rows) => format!("read the source: {rows}"),
                    None => "worked from the catalog".to_string(),
                };
                prior.push(PriorUnitOutput {
                    label: u.phase_id().unwrap_or("unit").to_string(),
                    output,
                });
            }
        }
        packs
    };
    for call in every_write_shape() {
        let apps_packs = run(true, &call);
        assert!(!apps_packs.is_empty());
        for pack in &apps_packs {
            for m in MARKERS {
                assert!(
                    !pack.contains(m),
                    "marker {m} from {} reached a context pack:\n{pack}",
                    call.tool
                );
            }
        }
    }
    // The controls: off an Apps run the same write tools hand their answer to a seat (so D-7 is
    // what withholds it above); a read's answer reaches a seat of an Apps run and of any other run
    // (whether a read returns rows is crew's data gate, out of this engine test's scope).
    for call in [rest_write(), rest_destructive()] {
        assert!(
            run(false, &call)
                .iter()
                .any(|p| MARKERS.iter().any(|m| p.contains(m))),
            "the harness sees {}'s answer off an Apps run",
            call.tool
        );
    }
    for apps in [true, false] {
        assert!(
            run(apps, &rest_read())
                .iter()
                .any(|p| MARKERS.iter().any(|m| p.contains(m))),
            "the harness sees a read's answer (apps: {apps})"
        );
    }
    let _ = std::fs::remove_dir_all(&base);
}

// ─── QE skips on a publish ──────────────────────────────────────────────────────────────────────

fn qe_denied_unit(session_id: &str, qe: Option<a::QeAcceptance>) -> WorkUnit {
    let mut u = WorkUnit::pending(format!("{session_id}:qe"), session_id, 2, "qe");
    u.repo_checks_floor = true;
    u.role = PhaseRole::Evaluator;
    u.status = UnitStatus::Rejected;
    u.denial = Some(UnitDenial::new(
        crate::qe_acceptance::review::DENIAL_SOURCE_QE_REVIEW,
        "QE reviewer FAIL",
    ));
    u.denials_behind = Some(Vec::new());
    u.qe_acceptance = qe;
    u
}

/// Acceptance (gate half): `skip_qe` is refused on a publish, and its gate never offers it — while
/// the same gate of any other QE-required run takes it.
#[test]
fn a_qe_skip_on_publish_is_refused_at_the_gate() {
    let mut s = session("r-pub", apps_contract(APPS_PUBLISH));
    s.status = SessionStatus::AwaitingHuman;
    let u = qe_denied_unit("r-pub", s.assurance.qe.clone());
    let err = crate::actor::skip_qe_plan(&s, Some(&u), "ship it", false, "r-pub")
        .unwrap_err()
        .to_string();
    assert!(err.contains("cannot be skipped"), "{err}");
    assert!(!crate::actor::skip_qe_takes_unit(&u), "never offered");

    let declared = vec![
        a::DISTINCT_EVALUATOR.to_string(),
        a::JUDGE.to_string(),
        a::QE_ACCEPTANCE.to_string(),
    ];
    let mut plain = session("r-feat", RunAssurance::new(Some(&declared), false));
    plain.status = SessionStatus::AwaitingHuman;
    let u = qe_denied_unit("r-feat", plain.assurance.qe.clone());
    assert_eq!(
        crate::actor::skip_qe_plan(&plain, Some(&u), "ship it", false, "r-feat").unwrap(),
        crate::actor::SkipQePlan::UnitCounts
    );
    assert!(crate::actor::skip_qe_takes_unit(&u));
}

/// On a publish, the operator's approve where no QE reviewer could judge is a retry, never the
/// judgment (that would be a QE skip); elsewhere it stays the judgment.
#[test]
fn no_operator_stands_in_for_a_publishs_qe_reviewer() {
    let publish = session("r-pub", apps_contract(APPS_PUBLISH));
    let mut u = qe_denied_unit("r-pub", publish.assurance.qe.clone());
    for source in [
        crate::qe_acceptance::review::DENIAL_SOURCE_QE_REVIEWER_UNAVAILABLE,
        crate::qe_acceptance::review::DENIAL_SOURCE_QE_INCONCLUSIVE,
    ] {
        u.denial = Some(UnitDenial::new(source, "no reviewer could judge"));
        assert!(!crate::actor::approve_is_operator_judgment(&publish, &u));
        let other = session("r-feat", RunAssurance::default());
        assert!(crate::actor::approve_is_operator_judgment(&other, &u));
    }
    // A semantic judge that could not rule is still the operator's to judge (not a QE layer).
    u.denial = Some(UnitDenial::new(
        crate::domain::DENIAL_SOURCE_JUDGE_UNAVAILABLE,
        "no judge",
    ));
    assert!(crate::actor::approve_is_operator_judgment(&publish, &u));
}

fn reviewed_by(seat: &str, basis: &str) -> crate::domain::QeReviewAttempt {
    crate::domain::QeReviewAttempt {
        seat: seat.into(),
        basis: basis.into(),
        same_cli: false,
        status: "verdict".into(),
        session: "nonce".into(),
        governed: true,
        tool_activity: false,
        attempt: 0,
    }
}

/// A bound QE PASS on tree `t1`, reviewed on `codex` (the QE executor runs on `claude`).
fn pass_review() -> QeReview {
    QeReview {
        outcome: crate::qe_acceptance::review::OUTCOME_PASS.into(),
        reason: "PASS".into(),
        evidence_dir: Some("/ev/qe-1".into()),
        qe_run_id: Some("qe-1".into()),
        tree: Some("t1".into()),
        verdict: Some("PASS".into()),
        attempts: vec![reviewed_by(
            "codex",
            crate::qe_acceptance::review::BASIS_DISTINCT,
        )],
    }
}

/// The backstop's rule: produce only on a required decision with a bound QE PASS.
#[test]
fn the_publish_consent_needs_a_bound_qe_pass() {
    let s = session("r-pub", apps_contract(APPS_PUBLISH));
    let mut qe = qe_denied_unit("r-pub", s.assurance.qe.clone());
    qe.status = UnitStatus::Done;
    qe.denial = None;
    qe.assigned_cli = Some("claude".into());
    let t1 = Some("t1");
    let why = publish_consent_refusal(&s, &[qe.clone()], t1).unwrap();
    assert!(why.contains("no reviewer record"), "{why}");
    qe.qe_review = Some(QeReview {
        outcome: crate::qe_acceptance::review::OUTCOME_INCONCLUSIVE.into(),
        verdict: None,
        ..pass_review()
    });
    assert!(publish_consent_refusal(&s, &[qe.clone()], t1).is_some());
    qe.qe_review = Some(pass_review());
    assert_eq!(publish_consent_refusal(&s, &[qe.clone()], t1), None);
    // pi r1: a PASS its executor's own seat reviewed (the `executor_seat` stand-in) refuses.
    qe.qe_review = Some(QeReview {
        attempts: vec![reviewed_by(
            "claude",
            crate::qe_acceptance::review::BASIS_EXECUTOR,
        )],
        ..pass_review()
    });
    let why = publish_consent_refusal(&s, &[qe.clone()], t1).unwrap();
    assert!(why.contains("distinct from the QE executor"), "{why}");
    qe.qe_review = Some(pass_review());
    // codex r1: a PASS taken on another tree than the run holds now refuses, and so does an
    // unreadable tree.
    let why = publish_consent_refusal(&s, &[qe.clone()], Some("t2")).unwrap();
    assert!(why.contains("accepted t1 while the run holds t2"), "{why}");
    assert!(publish_consent_refusal(&s, &[qe.clone()], None).is_some());
    // A decision that is not `required` (however it got there) refuses, PASS or not.
    let mut skipped = s.clone();
    if let Some(q) = skipped.assurance.qe.as_mut() {
        q.status = a::QE_SKIPPED.into();
    }
    assert!(publish_consent_refusal(&skipped, &[qe.clone()], t1).is_some());
    // No other run is judged here.
    let other = session("r-feat", RunAssurance::default());
    assert_eq!(publish_consent_refusal(&other, &[], None), None);
}

/// The backstop where it acts: the produce step's consent gate refuses an approve (and a choice)
/// without a bound QE PASS, and takes one with it.
#[test]
fn the_produce_consent_gate_refuses_an_approve_without_a_qe_pass() {
    use std::sync::mpsc::channel;
    let mut store = open_store(Some(":memory:")).unwrap();
    let run_id = "r-consent";
    let mut s = session(run_id, apps_contract(APPS_PUBLISH));
    s.unit_ix = 2;
    let mut check = WorkUnit::pending(format!("{run_id}:check"), run_id, 1, "check");
    check.tool_cmd = Some(vec!["true".into()]);
    check.status = UnitStatus::Done;
    let mut qe = qe_denied_unit(run_id, s.assurance.qe.clone());
    qe.status = UnitStatus::Done;
    qe.denial = None;
    let mut produce = WorkUnit::pending(format!("{run_id}:produce"), run_id, 3, "produce");
    produce.gate = GateSpec::ConsentBefore;
    produce.tool_cmd = Some(vec!["true".into()]);
    for u in [&check, &qe, &produce] {
        crate::domain::put_node(&mut store, u.to_node()).unwrap();
    }
    let mut subs = crate::event_log::EventSink::default();
    let (tx, _rx) = channel::<crate::command::Command>();
    crate::actor::pause_for_human(
        &mut store,
        &mut subs,
        &tx,
        &mut s,
        3,
        None,
        crate::actor::CONSENT_GATE_KIND,
        "produce?".into(),
    )
    .unwrap();
    struct Noop;
    impl crate::workflow::StepRunner for Noop {
        fn run_unit(&self, i: &StepInput) -> crate::workflow::StepOutput {
            crate::workflow::StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: String::new(),
                status: crate::workflow::StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }
    let runner: std::sync::Arc<dyn crate::workflow::StepRunner> = std::sync::Arc::new(Noop);
    let gate = |store: &mut dyn GraphStore,
                subs: &mut crate::event_log::EventSink,
                d: crate::workflow::HumanDecision| {
        let mut in_flight = std::collections::HashSet::new();
        crate::actor::confirm_gate(
            store,
            subs,
            &runner,
            &tx,
            &mut in_flight,
            run_id,
            d,
            &None,
            &None,
            uuid::Uuid::nil(),
            false,
        )
    };
    for d in [
        crate::workflow::HumanDecision::Approve {
            amend: None,
            amend_scope: crate::workflow::AmendScope::Cursor,
        },
        crate::workflow::HumanDecision::ConsentChoice {
            choice: "consent:site".into(),
        },
    ] {
        let err = gate(&mut store, &mut subs, d).unwrap_err().to_string();
        assert!(err.contains("takes only reject"), "{err}");
    }
    assert_eq!(
        crate::domain::get_session(&store, run_id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::AwaitingHuman,
        "nothing ran"
    );
    // codex r1: every answer but reject is refused there — an intent amendment would otherwise
    // release produce through the ordinary resume.
    let err = gate(
        &mut store,
        &mut subs,
        crate::workflow::HumanDecision::AmendIntent {
            text: "ship it anyway".into(),
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("takes only reject"), "{err}");
    // A bound PASS on a run with no readable tree still refuses (the positive path, on a real
    // worktree, is `tests/apps_presets.rs`).
    qe.qe_review = Some(pass_review());
    crate::domain::put_node(&mut store, qe.to_node()).unwrap();
    let err = gate(
        &mut store,
        &mut subs,
        crate::workflow::HumanDecision::Approve {
            amend: None,
            amend_scope: crate::workflow::AmendScope::Cursor,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("an unreadable tree"), "{err}");
    gate(
        &mut store,
        &mut subs,
        crate::workflow::HumanDecision::Reject,
    )
    .expect("reject stops it");
}

/// The builder exclusion survives the operator's levers: an explicit reassign to an excluded seat
/// is refused, and the `{cli: null}` re-route (which clears every other bench) keeps it.
#[test]
fn no_operator_reassign_seats_a_publish_on_an_excluded_builder() {
    let apps = AppsContract {
        preset: APPS_PUBLISH.into(),
    };
    let mut s = session("r-pub", apps_contract(APPS_PUBLISH));
    let signed_out = BenchedSeat {
        cli: "codex".into(),
        reason: "signed out".into(),
        source: "launcher".into(),
        governed_only: false,
        benched_until: None,
        strikes: 0,
        replaced: None,
    };
    let clis = [cli("claude"), cli("codex")];
    s.benched_seats = with_exclusion_benches(
        Some(&apps),
        &["claude".to_string()],
        &clis,
        vec![signed_out],
    );
    let why = reassign_refusal(&s, "claude").expect("refused");
    assert!(why.contains("built an Apps turn"), "{why}");
    assert!(!why.contains("   "), "one line of words: {why}");
    assert_eq!(
        reassign_refusal(&s, "codex"),
        None,
        "an ordinary bench is the operator's"
    );
    // codex r2: the launch's own list binds too — a seat outside the roster (no bench) and an
    // instance of an excluded CLI added since.
    s.exclude_seats = vec!["claude".into(), "opencode".into()];
    assert!(reassign_refusal(&s, "opencode").is_some(), "off-roster");
    assert!(reassign_refusal(&s, "claude#3").is_some(), "a new instance");
    assert_eq!(reassign_refusal(&s, "pi"), None);
    let kept = binding_benches(&s.benched_seats);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].cli, "claude");
    // The re-route over that bench never seats the excluded builder.
    let units = crate::plan::plan_from_def(&def_of(&seeded(), APPS_PUBLISH), "p", "r-pub");
    let dists =
        crate::distribute::distribute_units_on_benched(&units, &clis, "r-pub", &kept, &s.assurance)
            .unwrap();
    for (u, d) in units.iter().zip(dists) {
        if u.tool_cmd.is_none() {
            assert_ne!(d.assigned_cli, "claude", "{:?}", u.phase_id());
        }
    }
}

/// A turn lands only on a passing review: at the capped review's adjudication gate, the approve
/// that would land it over the reviewer's FAIL ("land with carried items") is refused.
#[test]
fn an_apps_turn_never_lands_over_a_failed_review() {
    use std::sync::mpsc::channel;
    let mut store = open_store(Some(":memory:")).unwrap();
    let run_id = "r-adjudicate";
    let mut s = session(run_id, apps_contract(APPS_TURN));
    s.unit_ix = 2;
    let mut build = WorkUnit::pending(format!("{run_id}:build"), run_id, 1, "build");
    build.role = PhaseRole::Creator;
    build.status = UnitStatus::Done;
    build.assigned_cli = Some("claude".into());
    let mut check = WorkUnit::pending(format!("{run_id}:check"), run_id, 2, "check");
    check.tool_cmd = Some(vec!["true".into()]);
    check.status = UnitStatus::Done;
    let mut review = WorkUnit::pending(format!("{run_id}:review"), run_id, 3, "review");
    review.role = PhaseRole::Evaluator;
    review.status = UnitStatus::Rejected;
    review.assigned_cli = Some("codex".into());
    review.denial = Some(UnitDenial::new("evaluator", "VERDICT: FAIL"));
    for u in [&build, &check, &review] {
        crate::domain::put_node(&mut store, u.to_node()).unwrap();
    }
    let mut subs = crate::event_log::EventSink::default();
    let (tx, _rx) = channel::<crate::command::Command>();
    crate::actor::pause_for_human(
        &mut store,
        &mut subs,
        &tx,
        &mut s,
        3,
        None,
        crate::review_context::REVIEW_ADJUDICATION_GATE,
        "land with carried items?".into(),
    )
    .unwrap();
    struct Noop;
    impl crate::workflow::StepRunner for Noop {
        fn run_unit(&self, i: &StepInput) -> crate::workflow::StepOutput {
            crate::workflow::StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: String::new(),
                status: crate::workflow::StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }
    let runner: std::sync::Arc<dyn crate::workflow::StepRunner> = std::sync::Arc::new(Noop);
    let mut in_flight = std::collections::HashSet::new();
    let err = crate::actor::confirm_gate(
        &mut store,
        &mut subs,
        &runner,
        &tx,
        &mut in_flight,
        run_id,
        crate::workflow::HumanDecision::Approve {
            amend: None,
            amend_scope: crate::workflow::AmendScope::Cursor,
        },
        &None,
        &None,
        uuid::Uuid::nil(),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("lands only on a passing review"), "{err}");
    assert_eq!(
        crate::domain::get_session(&store, run_id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::AwaitingHuman
    );
    // codex r2: refused before the gate resolves — the adjudication row is still open, for request
    // changes or reject.
    let open = crate::interaction::list_interactions(
        &store,
        Some(run_id),
        Some(crate::interaction::InteractionStatus::Open),
    )
    .unwrap();
    assert!(
        open.iter().any(
            |r| r.gate_kind.as_deref() == Some(crate::review_context::REVIEW_ADJUDICATION_GATE)
        ),
        "{open:?}"
    );
}

/// codex r2: the dispatch guard's wiring. An approve at a publish's QE escalation gate re-dispatches
/// the QE unit through `dispatch_unit`; on an engine that cannot run it contained and governed (this
/// test thread arms no input governance), the dispatch is refused and the run FAILS with the reason
/// — never left executing with no worker.
#[test]
fn a_refused_apps_dispatch_fails_the_run() {
    use std::sync::mpsc::channel;
    let _env = crate::test_env::ENV_LOCK.read();
    let mut store = open_store(Some(":memory:")).unwrap();
    let run_id = "r-refused";
    let mut s = session(run_id, apps_contract(APPS_PUBLISH));
    s.unit_ix = 1;
    let mut check = WorkUnit::pending(format!("{run_id}:check"), run_id, 1, "check");
    check.tool_cmd = Some(vec!["true".into()]);
    check.status = UnitStatus::Done;
    let mut qe = qe_denied_unit(run_id, s.assurance.qe.clone());
    qe.assigned_cli = Some("codex".into());
    qe.last_attempt = Some(0);
    let mut produce = WorkUnit::pending(format!("{run_id}:produce"), run_id, 3, "produce");
    produce.gate = GateSpec::ConsentBefore;
    produce.tool_cmd = Some(vec!["true".into()]);
    for u in [&check, &qe, &produce] {
        crate::domain::put_node(&mut store, u.to_node()).unwrap();
    }
    let mut subs = crate::event_log::EventSink::default();
    let (tx, _rx) = channel::<crate::command::Command>();
    crate::actor::pause_for_human(
        &mut store,
        &mut subs,
        &tx,
        &mut s,
        2,
        None,
        crate::actor::ESCALATION_GATE_KIND,
        "the QE reviewer failed it".into(),
    )
    .unwrap();
    struct Noop;
    impl crate::workflow::StepRunner for Noop {
        fn run_unit(&self, i: &StepInput) -> crate::workflow::StepOutput {
            crate::workflow::StepOutput {
                run_id: i.run_id.clone(),
                unit_ix: i.unit_ix,
                attempt: i.attempt,
                output: String::new(),
                status: crate::workflow::StepStatus::Ok,
                usage: None,
                files: Vec::new(),
                tools: Vec::new(),
                governed: false,
            }
        }
    }
    let runner: std::sync::Arc<dyn crate::workflow::StepRunner> = std::sync::Arc::new(Noop);
    let mut in_flight = std::collections::HashSet::new();
    let err = crate::actor::confirm_gate(
        &mut store,
        &mut subs,
        &runner,
        &tx,
        &mut in_flight,
        run_id,
        crate::workflow::HumanDecision::Approve {
            amend: None,
            amend_scope: crate::workflow::AmendScope::Cursor,
        },
        &None,
        &None,
        uuid::Uuid::nil(),
        false,
    )
    .unwrap_err()
    .to_string();
    assert!(err.contains("is refused"), "{err}");
    assert_eq!(
        crate::domain::get_session(&store, run_id)
            .unwrap()
            .unwrap()
            .status,
        SessionStatus::Failed
    );
}

/// codex r1: THE dispatch guard, on every path that reaches a unit: an Apps agent unit is refused
/// off the container floor and without input governance, a publish's unit on an excluded builder
/// seat is refused, and a publish's produce step dispatches only past the QE backstop.
#[test]
fn the_apps_dispatch_guard() {
    let store = seeded();
    let mut s = session("r-pub", apps_contract(APPS_PUBLISH));
    let apps = AppsContract {
        preset: APPS_PUBLISH.into(),
    };
    s.benched_seats = with_exclusion_benches(
        Some(&apps),
        &["claude".to_string()],
        &[cli("claude"), cli("codex"), cli("pi")],
        Vec::new(),
    );
    let mut units = crate::plan::plan_from_def(&def_of(&store, APPS_PUBLISH), "p", "r-pub");
    let qe_ix = units
        .iter()
        .position(|u| u.phase_id() == Some(QE_STEP))
        .unwrap();
    // The QE executor runs on pi; `pass_review` was reviewed on codex.
    units[qe_ix].assigned_cli = Some("pi".into());
    let qe = units[qe_ix].clone();
    let produce = units
        .iter()
        .find(|u| u.phase_id() == Some(PRODUCE_STEP))
        .unwrap()
        .clone();
    let check = units
        .iter()
        .find(|u| u.phase_id() == Some(CHECK_STEP))
        .unwrap()
        .clone();
    let tree = || Some("t1".to_string());
    let ok = |u: &WorkUnit, units: &[WorkUnit]| {
        dispatch_refusal(&s, units, u, Some("container"), true, tree)
    };
    assert_eq!(ok(&qe, &units), None);
    assert_eq!(ok(&check, &units), None, "the check precedes the QE unit");
    assert!(dispatch_refusal(&s, &units, &qe, Some("host"), true, tree)
        .unwrap()
        .contains("container floor"));
    assert!(
        dispatch_refusal(&s, &units, &qe, Some("container"), false, tree)
            .unwrap()
            .contains("input governance")
    );
    let mut on_builder = qe.clone();
    on_builder.assigned_cli = Some("claude".into());
    assert!(ok(&on_builder, &units)
        .unwrap()
        .contains("built an Apps turn"));
    // (core#1390) produce is where the guard reads the tree — and so where the dispatch probe
    // reads it off the actor; nowhere else on this run.
    assert!(reads_publish_tree(&s, &units, &produce));
    assert!(
        !reads_publish_tree(&s, &units, &qe),
        "an agent unit reads no tree here"
    );
    assert!(
        !reads_publish_tree(&s, &units, &check),
        "the check precedes the QE unit"
    );
    // produce: refused without the bound PASS, dispatched with it on the same tree.
    assert!(ok(&produce, &units).unwrap().contains("bound QE PASS"));
    units[qe_ix].qe_review = Some(pass_review());
    assert_eq!(ok(&produce, &units), None);
    assert!(
        dispatch_refusal(&s, &units, &produce, Some("container"), true, || {
            Some("t2".into())
        })
        .is_some()
    );
    // No other run is judged here.
    let other = session("r-feat", RunAssurance::default());
    assert_eq!(
        dispatch_refusal(&other, &units, &qe, Some("host"), false, tree),
        None
    );
}

/// codex r1: the straight-through driver (`Core::launch`, the operator CLI) runs no Tool, gate or
/// consent, so it refuses an Apps preset instead of reporting a run it never checked.
#[test]
fn the_straight_through_driver_refuses_an_apps_preset() {
    for name in [APPS_TURN, APPS_PUBLISH] {
        let mut store = seeded();
        let err = crate::pipeline::run_session(
            &mut store,
            vec![cli("claude"), cli("codex")],
            "p",
            EntityMode::Shared,
            "r-straight",
            Some(name),
            &mut |_| {},
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("launch_run"), "{name}: {err}");
    }
}

/// codex r3: the produce step's consent card comes from the trusted check alone. A dry-run plan the
/// (untrusted) QE seat printed is never the offer, even when the check printed one too.
#[test]
fn the_publish_consent_card_is_the_checks_never_the_qe_seats() {
    let store_seed = seeded();
    let def = def_of(&store_seed, APPS_PUBLISH);
    let produce_def = def.phases.iter().find(|p| p.id == PRODUCE_STEP).unwrap();
    assert_eq!(produce_def.depends_on, [CHECK_STEP]);
    let mut store = open_store(Some(":memory:")).unwrap();
    let run_id = "r-card";
    crate::domain::put_node(
        &mut store,
        session(run_id, apps_contract(APPS_PUBLISH)).to_node(),
    )
    .unwrap();
    let units = crate::plan::plan_from_def(&def, "publish", run_id);
    let plan = |id: &str| {
        format!(
            "{{\"dry_run\":true,\"choices\":[{{\"id\":\"{id}\",\"label\":\"{id}\",\"default\":true,\"writes\":[]}}]}}"
        )
    };
    for u in &units {
        let mut u = u.clone();
        u.status = UnitStatus::Done;
        crate::domain::put_node(&mut store, u.to_node()).unwrap();
        let out = match u.phase_id() {
            Some(CHECK_STEP) => Some(plan("site")),
            Some(QE_STEP) => Some(format!("looks good\n{}", plan("everywhere"))),
            _ => None,
        };
        if let Some(out) = out {
            crate::execute::apply_unit(
                &mut store,
                &u,
                &out,
                &format!("wf-{run_id}"),
                EntityMode::Shared,
                run_id,
                None,
                0,
            )
            .unwrap();
        }
    }
    let produce = units
        .iter()
        .find(|u| u.phase_id() == Some(PRODUCE_STEP))
        .unwrap();
    let check = units
        .iter()
        .find(|u| u.phase_id() == Some(CHECK_STEP))
        .unwrap();
    let offer = crate::actor::consent_offer(&store, run_id, produce.ord).unwrap();
    assert_eq!(offer.plan_ord, Some(check.ord), "{offer:?}");
    let ids: Vec<String> = offer
        .plan
        .unwrap()
        .choices
        .iter()
        .map(|c| c.id.clone())
        .collect();
    assert_eq!(ids, ["site"]);
}

/// codex r3: a publish's QE evidence is never reviewed on the QE executor's own seat, not even as
/// the last fallback: with no distinct reviewer the unit is denied as unavailable (an approve
/// retries) instead of counting on a PASS the produce step would refuse. Elsewhere the executor
/// fallback stands.
#[test]
fn a_publish_has_no_executor_seat_qe_reviewer() {
    let mut s = session("r-pub", apps_contract(APPS_PUBLISH));
    s.clis = vec!["claude".into(), "codex".into(), "pi".into()];
    let apps = AppsContract {
        preset: APPS_PUBLISH.into(),
    };
    s.benched_seats = with_exclusion_benches(
        Some(&apps),
        &["claude".to_string()],
        &[cli("claude"), cli("codex"), cli("pi")],
        Vec::new(),
    );
    crate::domain::bench_seat(
        &mut s.benched_seats,
        BenchedSeat {
            cli: "pi".into(),
            reason: "quota_exhausted".into(),
            source: "worker".into(),
            governed_only: false,
            benched_until: None,
            strikes: 1,
            replaced: None,
        },
    );
    let mut qe = qe_denied_unit("r-pub", s.assurance.qe.clone());
    qe.assigned_cli = Some("codex".into());
    let units = vec![qe.clone()];
    let seats = |s: &AgentSession| -> Vec<(String, String)> {
        crate::qe_acceptance::review::candidates(s, &units, &qe)
            .into_iter()
            .map(|c| (c.seat, c.basis))
            .collect()
    };
    assert!(seats(&s).is_empty(), "{:?}", seats(&s));
    let mut other = s.clone();
    other.assurance = RunAssurance::default();
    assert!(seats(&other)
        .iter()
        .any(|(seat, basis)| seat == "codex"
            && basis == crate::qe_acceptance::review::BASIS_EXECUTOR));
}
