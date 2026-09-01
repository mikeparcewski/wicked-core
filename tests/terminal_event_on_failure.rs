//! FINDING-033 regression: a run that aborts BEFORE any unit is dispatched must still emit a
//! TERMINAL lifecycle event, not just a `CoreEvent::Error`.
//!
//! Observed live: launching a workflow whose phase pins a validator missing from the vault made the
//! actor bail (fail-closed, correctly) — but the `/ws` stream showed
//!
//!     sessionStarted → error → (silence)
//!
//! while `GET /runs/<id>` reported `"status": "failed"`. The store knew the run was terminal; the
//! stream never said so. `CoreEvent::Error` is NOT a terminal event — the actor also emits it for
//! non-fatal conditions on a run that keeps going — so a consumer cannot treat it as end-of-run and
//! is left waiting forever on a run that already ended.
//!
//! `pre_distribute` is the site that fired live (an unresolvable `validator_pin` is caught there, at
//! plan time), which makes it the honest reproduction; the fix routes it and its seven siblings
//! through `fail_run` so the store write and the terminal event cannot drift apart again.
//!
//! Scope note: only the standalone `LaunchRun` command path reaches these sites. A campaign node
//! launches through `launch_run_inner`, which plans SYNCHRONOUSLY and returns the error to
//! `campaign::dispatch` — that path reconciles the node itself and was never affected.

use std::sync::mpsc::RecvTimeoutError;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, LaunchSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus,
};

/// The workflow def whose only phase pins a validator that is NOT in the vault.
/// `attach_pinned_validators` is fail-closed on this — it refuses to run the phase ungated — so the
/// run aborts inside `pre_distribute`, before any unit is dispatched. This is the live FINDING-033
/// trigger, reproduced exactly.
const UNRESOLVABLE_PIN_DEF: &str = r#"{ "id": "unresolvable-pin",
     "phases": [ { "id": "build", "kind": "build", "validator_pin": "deadbeefdeadbeef" } ] }"#;

/// How long to wait for the terminal event. Generous: the failure is asynchronous (the actor replies
/// to `launch_run` before `pre_distribute` runs), and a timeout must read as a timeout, not as a
/// missing event.
const DEADLINE: Duration = Duration::from_secs(30);

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

/// Never actually reached — the run must die at plan time. Present so the engine has a runner.
struct OkRunner;
impl StepRunner for OkRunner {
    fn run_unit(&self, i: &StepInput) -> StepOutput {
        StepOutput {
            run_id: i.run_id.clone(),
            unit_ix: i.unit_ix,
            attempt: i.attempt,
            output: "ok".into(),
            status: StepStatus::Ok,
            usage: None,
            files: Vec::new(),
            tools: Vec::new(),
            governed: false,
        }
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

fn db_path(name: &str) -> String {
    let dir = std::env::temp_dir().join(format!("wicked-core-term-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("estate.db").to_str().unwrap().to_string()
}

/// A run that fails before dispatching a unit emits `SessionFailed` — the transition a stream
/// consumer needs — in addition to the `Error` that carries the human-readable reason.
#[test]
fn a_pre_dispatch_failure_emits_a_terminal_session_failed() {
    let core = Core::spawn_with_engine(
        db_path("pre-dispatch"),
        Arc::new(StubDispatcher),
        Arc::new(OkRunner),
    );

    core.register_workflow(UNRESOLVABLE_PIN_DEF)
        .expect("register the fail-closed workflow");

    // Subscribe BEFORE launching so the stream cannot race past the terminal event.
    let events = core.subscribe();

    let run_id = core
        .launch_run(LaunchSpec {
            project_id: None,
            problem: "this run must die at plan time".into(),
            clis: vec![cli("a")],
            entity_mode: EntityMode::Shared,
            session_id: "r".into(),
            human_confirm: HumanConfirm::None,
            repo_ref: None,
            workflow: Some("unresolvable-pin".into()),
            extra_write_roots: Vec::new(),
            project_graph: None,
        })
        // The actor replies to the caller BEFORE `pre_distribute` runs, so the launch itself
        // succeeds and the failure arrives asynchronously — exactly as the daemon saw it (HTTP 200,
        // then a dead stream).
        .expect("launch is accepted; the failure is asynchronous");
    assert_eq!(run_id, "r");

    let mut saw_error = false;
    let mut failed_ord = None;
    let deadline = Instant::now() + DEADLINE;
    // Collect BOTH signals before stopping. Bailing on the first `SessionFailed` would make the
    // `saw_error` assertion below depend on the current emission order (`Error` then `SessionFailed`);
    // this test is about which events exist, not the order they arrive in.
    while Instant::now() < deadline && !(saw_error && failed_ord.is_some()) {
        match events.recv_timeout(Duration::from_millis(200)) {
            Ok(CoreEvent::SessionFailed { session, ord }) if session == "r" => {
                failed_ord = Some(ord);
            }
            Ok(CoreEvent::Error { session, message }) if session.as_deref() == Some("r") => {
                assert!(
                    message.contains("deadbeefdeadbeef"),
                    "the Error must carry the fail-closed reason: {message}"
                );
                // A refusal the operator cannot act on is only half a message.
                assert!(
                    message.contains("wicked-core provision-validator"),
                    "the fail-closed reason must name a command that resolves it: {message}"
                );
                saw_error = true;
            }
            Ok(_) => {}
            Err(RecvTimeoutError::Timeout) => {}
            // The actor dropped the sender — no further event can arrive, so spinning to the
            // deadline would only turn an actor crash into a misleading "no SessionFailed" verdict.
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the event stream disconnected before the run reached a terminal state")
            }
        }
    }

    assert_eq!(
        failed_ord,
        Some(0),
        "a run that aborts before dispatching a unit must emit a TERMINAL SessionFailed \
         (ord 0 — no unit ran). Without it the stream goes silent on a run the store already \
         marked failed, and a consumer waits forever."
    );
    assert!(
        saw_error,
        "SessionFailed does not replace Error — the operator still needs the reason"
    );

    // The store agrees: the two halves that drifted apart now say the same thing.
    let views = core.sessions_detail().unwrap();
    let v = views.iter().find(|v| v.session.id == "r").unwrap();
    assert_eq!(v.session.status, SessionStatus::Failed);
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
