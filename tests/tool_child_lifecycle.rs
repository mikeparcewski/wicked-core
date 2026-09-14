//! core#500 / F-BM-008 — the Tool executor's child has a LIFECYCLE: `CancelRun` kills it.
//!
//! Run 6 of the BM journey: the operator pressed Cancel at 05:46; at 08:19 the *cancelled* run's
//! deliver script committed, pushed and opened a PR under the daemon's token. The tool carrier
//! held no launch identity (`launch_seq 0`), no handle and no group — `CancelRun`'s
//! `tombstone_run` + `advance_launch_seq` were signals it never read. These tests go through the
//! REAL engine (`Core::launch_run` → plan → dispatch → the tool thread) with a shell child that
//! backgrounds a second process, so the group kill is observed on real pids.
//!
//! POSIX only (`#![cfg(unix)]` — the suite's idiom for script-driven tests): the children are
//! `sh` scripts, liveness is probed with `kill(pid, 0)`, and the group kill is a unix contract
//! (Windows kills the leader only, as the ACP and wrapped carriers do).

#![cfg(unix)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use wicked_core::{
    Core, CoreEvent, EntityMode, HumanConfirm, HumanDecision, LaunchSpec, SessionStatus, StepInput,
    StepOutput, StepRunner, StepStatus,
};
use wicked_council::types::{Category, Confidence, Dispatcher, InputMode, Vote};
use wicked_council::{AgenticCli, CouncilTask};

/// Pre-main (single-threaded, so no test thread can race the env write): arm the hermetic emit
/// spool (core#311) so any emission this suite triggers lands in a per-process temp outbox, never
/// in the operator's real replay queue. `harness_hygiene` fails the suite without this block.
///
/// SAFETY (`ctor(unsafe)`): runs before `main` on one thread and only touches process env vars
/// via the std API — no allocator setup, no threads, no panics across the FFI boundary.
#[ctor::ctor(unsafe)]
fn arm_hermetic_emit_spool() {
    wicked_apps_core::emit::hermetic_test_spool();
}

// ── Shared helpers (the `events_governance_deep` harness) ──────────────────────────────────────

fn fixture_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "wicked-core-toolkill-{name}-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn db_path(dir: &std::path::Path) -> String {
    dir.join("estate.db").to_str().unwrap().to_string()
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
        health: None,
    }
}

struct NumericDispatcher;
impl Dispatcher for NumericDispatcher {
    fn dispatch(&self, c: &AgenticCli, _: &CouncilTask) -> Option<Vote> {
        Some(Vote {
            cli: c.key.clone(),
            recommendation: "1".into(),
            top_risk: "none".into(),
            change_my_mind: "no".into(),
            disqualifier: None,
            confidence: Confidence::default(),
            provenance: "numeric".into(),
        })
    }
}

/// Completes every agent unit immediately with Ok status.
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
            files: vec![],
            tools: Vec::new(),
            governed: false,
        }
    }
}

fn spec(sid: &str, workflow: &str) -> LaunchSpec {
    LaunchSpec {
        base_ref: None,
        project_id: None,
        problem: "Run the tool.".into(),
        clis: vec![cli("stub")],
        entity_mode: EntityMode::Shared,
        session_id: sid.into(),
        human_confirm: HumanConfirm::None,
        auto_deliver: false,
        repo_ref: None,
        workflow: Some(workflow.into()),
        extra_write_roots: Vec::new(),
        extra_read_roots: Vec::new(),
        project_graph: None,
    }
}

/// Collect events until `until` matches one (inclusive) or `within` elapses.
fn collect_until(
    events: &std::sync::mpsc::Receiver<CoreEvent>,
    within: Duration,
    until: impl Fn(&CoreEvent) -> bool,
) -> Vec<CoreEvent> {
    let mut out = Vec::new();
    let deadline = Instant::now() + within;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::ZERO);
        if remaining.is_zero() {
            break;
        }
        match events.recv_timeout(remaining.min(Duration::from_millis(200))) {
            Ok(ev) => {
                let done = until(&ev);
                out.push(ev);
                if done {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
        }
    }
    out
}

/// Signal 0 probes existence only.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

fn wait_dead(pid: i32, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    while Instant::now() < deadline {
        if !alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

/// The pid a script wrote to `path` (polled — the child writes it a few ms after the spawn).
fn read_pid(path: &std::path::Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse::<i32>().ok())
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the tool child never wrote its pid to {}", path.display());
}

fn tool_def(id: &str, script: &str) -> String {
    serde_json::json!({
        "id": id,
        "phases": [ {
            "id": "run",
            "kind": "build",
            "executor": { "type": "tool", "cmd": ["sh", "-c", script] }
        } ]
    })
    .to_string()
}

// ── (1) Cancel kills the live tool child AND its group; the frame follows runCancelled ─────────

/// Design test (1): a tool unit `sleep 300 & sleep 300` is DISPATCHED, `CancelRun` lands, and
/// within seconds BOTH the leader and the backgrounded sleep are gone (group kill); on the wire
/// `runCancelled` PRECEDES `toolExecutorKilled{ord:1, attempt:0}` — the ORDER is the contract,
/// `reason` is whichever invalidation signal the child observed; no `unitOutputCaptured` for the
/// killed attempt and no second terminal frame.
#[test]
fn cancel_run_kills_the_live_tool_child_and_its_group_and_the_frame_follows_run_cancelled() {
    let dir = fixture_dir("cancel");
    let leader = dir.join("leader.pid");
    let bg = dir.join("bg.pid");
    let script = format!(
        "echo $$ > '{}'; sleep 300 & echo $! > '{}'; wait",
        leader.display(),
        bg.display()
    );
    let sid = "toolkill-cancel";
    let core = Core::spawn_with_engine(
        db_path(&dir),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(tool_def("tool-kill", &script))
        .unwrap();
    core.launch_run(spec(sid, "tool-kill")).expect("launch");

    // 1. The tool unit was dispatched and both processes exist.
    let pre = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid),
    );
    assert!(
        pre.iter().any(
            |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid)
        ),
        "the tool unit dispatched: {pre:?}"
    );
    let leader_pid = read_pid(&leader);
    let bg_pid = read_pid(&bg);
    assert!(
        alive(leader_pid),
        "leader {leader_pid} runs before the cancel"
    );
    assert!(
        alive(bg_pid),
        "backgrounded sleep {bg_pid} runs before the cancel"
    );

    // 2. The operator cancels.
    let cancelled_at = Instant::now();
    let status = core.cancel_run(sid).expect("cancel");
    assert_eq!(status, SessionStatus::Cancelled);

    // 3. The kill frame follows runCancelled — within 5 s (one 50 ms poll tick + a bounded reap).
    let post = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::ToolExecutorKilled { session, .. } if session == sid),
    );
    let idx_cancelled = post
        .iter()
        .position(|e| matches!(e, CoreEvent::RunCancelled { session } if session == sid))
        .expect("runCancelled on the wire");
    let idx_killed = post
        .iter()
        .position(|e| matches!(e, CoreEvent::ToolExecutorKilled { session, .. } if session == sid))
        .unwrap_or_else(|| panic!("toolExecutorKilled within 5 s of the cancel; got {post:?}"));
    assert!(
        idx_cancelled < idx_killed,
        "ORDER is the contract: runCancelled before toolExecutorKilled — {post:?}"
    );
    match &post[idx_killed] {
        CoreEvent::ToolExecutorKilled {
            ord,
            attempt,
            pid,
            reason,
            ran_ms,
            ..
        } => {
            assert_eq!(*ord, 1);
            assert_eq!(*attempt, 0, "the dispatched attempt");
            assert_eq!(
                *pid as i32, leader_pid,
                "the killed leader is the spawned child"
            );
            assert!(
                matches!(reason.as_str(), "cancelled" | "superseded"),
                "the signal the child observed (never `shutdown` here): {reason}"
            );
            assert!(*ran_ms > 0);
        }
        other => unreachable!("{other:?}"),
    }
    assert!(
        cancelled_at.elapsed() < Duration::from_secs(5),
        "the frame arrived within 5 s of the cancel"
    );

    // 4. Both processes are dead — the GROUP was killed, not just the leader.
    assert!(
        wait_dead(leader_pid, Duration::from_secs(2)),
        "leader {leader_pid} still alive after the cancel"
    );
    assert!(
        wait_dead(bg_pid, Duration::from_secs(2)),
        "the backgrounded sleep {bg_pid} survived — the group was not killed"
    );

    // 5. No output capture for the killed attempt and no second terminal frame: the killed
    //    child's `Cancelled` result is discarded by the existing terminal guard.
    let mut all = pre;
    all.extend(post);
    all.extend(collect_until(&ev, Duration::from_millis(600), |_| false));
    assert!(
        !all.iter()
            .any(|e| matches!(e, CoreEvent::UnitOutputCaptured { session, .. } if session == sid)),
        "no unitOutputCaptured for a killed attempt: {all:?}"
    );
    let terminals = all
        .iter()
        .filter(|e| {
            matches!(e, CoreEvent::RunCancelled { session } if session == sid)
                || matches!(e, CoreEvent::SessionCompleted { session } if session == sid)
                || matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)
        })
        .count();
    assert_eq!(
        terminals, 1,
        "exactly one terminal frame (runCancelled): {all:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── (6) Reject at a def gate before a tool unit: nothing to kill, nothing dispatched ────────────

/// Design test (6) — the never-run invariant: a gate is raised only pre-dispatch or after the
/// cursor's result folded, so NO tool child is live at a gate. Reject on a def gate whose next
/// unit is a tool `sleep 300` → `runCancelled`, 0 `toolExecutorDispatched`, 0 `toolExecutorKilled`.
#[test]
fn reject_at_a_def_gate_before_a_tool_unit_kills_nothing_and_dispatches_nothing() {
    let dir = fixture_dir("reject");
    let sid = "toolkill-reject";
    let def_json = serde_json::json!({
        "id": "gate-then-tool",
        "phases": [
            { "id": "plan", "kind": "recon",
              "gate": { "human_confirm": { "unconditional": true } } },
            { "id": "deliver", "kind": "build", "depends_on": ["plan"],
              "executor": { "type": "tool", "cmd": ["sh", "-c", "sleep 300"] } }
        ]
    })
    .to_string();
    let core = Core::spawn_with_engine(
        db_path(&dir),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(def_json).unwrap();
    core.launch_run(spec(sid, "gate-then-tool"))
        .expect("launch");

    // The agent unit completes; the def gate pauses the run BEFORE the tool unit dispatches.
    let pre = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid),
    );
    assert!(
        pre.iter()
            .any(|e| matches!(e, CoreEvent::AwaitingHuman { session, .. } if session == sid)),
        "the run pauses at the def gate: {pre:?}"
    );

    // Reject → cancel. Nothing was running, so nothing is killed.
    let status = core
        .confirm_gate(sid, HumanDecision::Reject)
        .expect("reject");
    assert_eq!(status, SessionStatus::Cancelled);
    let post = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::RunCancelled { session } if session == sid),
    );
    let mut all = pre;
    all.extend(post);
    all.extend(collect_until(&ev, Duration::from_millis(600), |_| false));
    assert!(
        all.iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { session } if session == sid)),
        "reject cancels: {all:?}"
    );
    assert!(
        !all.iter().any(
            |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid)
        ),
        "the tool unit never dispatched: {all:?}"
    );
    assert!(
        !all.iter()
            .any(|e| matches!(e, CoreEvent::ToolExecutorKilled { session, .. } if session == sid)),
        "nothing to kill at a gate, so no kill frame: {all:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
