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
    CampaignDef, CampaignNode, CampaignStatus, Core, CoreEvent, EntityMode, FailurePolicy,
    HumanConfirm, HumanDecision, LaunchSpec, RunSpec, SessionStatus, StepInput, StepOutput,
    StepRunner, StepStatus,
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
        plan: None,
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
    //    AC2: runCancelled must carry tool_children_killed >= 1 (the actor killed the group
    //    synchronously BEFORE emitting the event).
    let post = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::ToolExecutorKilled { session, .. } if session == sid),
    );
    let idx_cancelled = post
        .iter()
        .position(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid))
        .expect("runCancelled on the wire");
    let idx_killed = post
        .iter()
        .position(|e| matches!(e, CoreEvent::ToolExecutorKilled { session, .. } if session == sid))
        .unwrap_or_else(|| panic!("toolExecutorKilled within 5 s of the cancel; got {post:?}"));
    assert!(
        idx_cancelled < idx_killed,
        "ORDER is the contract: runCancelled before toolExecutorKilled — {post:?}"
    );
    // AC2: runCancelled carries tool_children_killed >= 1.
    if let CoreEvent::RunCancelled {
        tool_children_killed,
        ..
    } = &post[idx_cancelled]
    {
        assert!(
            *tool_children_killed >= 1,
            "runCancelled.toolChildrenKilled must be >= 1 when a tool child was live: {post:?}"
        );
    }
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
            assert_eq!(
                reason.as_str(),
                "cancelled",
                "fix #5: tombstone persists after cancel so the child always observes 'cancelled', never 'superseded'"
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
    //    AC3 (discarded path): `ToolResultDiscarded{reason:"cancelled"}` fires after the killed
    //    attempt's ApplyStepResult is received.
    let mut all = pre;
    all.extend(post);
    all.extend(collect_until(
        &ev,
        Duration::from_millis(600),
        |e| matches!(e, CoreEvent::ToolResultDiscarded { session, .. } if session == sid),
    ));
    all.extend(collect_until(&ev, Duration::from_millis(200), |_| false));
    assert!(
        !all.iter()
            .any(|e| matches!(e, CoreEvent::UnitOutputCaptured { session, .. } if session == sid)),
        "no unitOutputCaptured for a killed attempt: {all:?}"
    );
    let terminals = all
        .iter()
        .filter(|e| {
            matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid)
                || matches!(e, CoreEvent::SessionCompleted { session } if session == sid)
                || matches!(e, CoreEvent::SessionFailed { session, .. } if session == sid)
        })
        .count();
    assert_eq!(
        terminals, 1,
        "exactly one terminal frame (runCancelled): {all:?}"
    );
    // AC3: ToolResultDiscarded fires for the killed attempt's Cancelled result.
    assert!(
        all.iter().any(|e| matches!(
            e,
            CoreEvent::ToolResultDiscarded { session, reason, .. }
                if session == sid && reason == "cancelled"
        )),
        "ToolResultDiscarded{{reason:cancelled}} must fire when the killed result posts back: {all:?}"
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
        |e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid),
    );
    let mut all = pre;
    all.extend(post);
    all.extend(collect_until(&ev, Duration::from_millis(600), |_| false));
    assert!(
        all.iter()
            .any(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid)),
        "reject cancels: {all:?}"
    );
    // AC2: tool_children_killed == 0 when no tool unit was running at the gate.
    if let Some(CoreEvent::RunCancelled {
        tool_children_killed,
        ..
    }) = all
        .iter()
        .find(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid))
    {
        assert_eq!(
            *tool_children_killed, 0,
            "no tool child was running at the gate — killed count must be 0: {all:?}"
        );
    }
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

// ── (2) Cancel kills the full process group including grandchildren ────────────────────────────

/// Design test (2) AC3: a deliver-shaped script backgrounds a grandchild that would create a
/// sentinel file after sleeping (proxy for any irreversible side-effect). The run is cancelled
/// while both processes are alive. Both the leader and the grandchild must be dead within 5 s,
/// and the sentinel must NOT exist (the grandchild was killed before it could act).
/// `runCancelled.toolChildrenKilled >= 1` is on the wire.
#[test]
fn cancel_kills_entire_process_group_including_grandchild() {
    let dir = fixture_dir("grandchild");
    let leader = dir.join("leader.pid");
    let bg = dir.join("bg.pid");
    let sentinel = dir.join("sentinel.flag");
    // sleep 1 so the grandchild creates the sentinel well within the post-cancel wait window
    // if the group kill fails — making the sentinel assertion genuinely falsifiable.
    let script = format!(
        "echo $$ > '{ldr}'; \
         (sleep 1 && touch '{sen}') & \
         echo $! > '{bg}'; \
         sleep 300",
        ldr = leader.display(),
        sen = sentinel.display(),
        bg = bg.display(),
    );

    let sid = "toolkill-grandchild";
    let core = Core::spawn_with_engine(
        db_path(&dir),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(tool_def("grandchild-tool", &script))
        .unwrap();
    core.launch_run(spec(sid, "grandchild-tool"))
        .expect("launch");

    let pre = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid),
    );
    assert!(
        pre.iter().any(
            |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid)
        ),
        "tool dispatched: {pre:?}"
    );
    let leader_pid = read_pid(&leader);
    let bg_pid = read_pid(&bg);
    assert!(alive(leader_pid), "leader alive before cancel");
    assert!(alive(bg_pid), "grandchild alive before cancel");

    let status = core.cancel_run(sid).expect("cancel");
    assert_eq!(status, SessionStatus::Cancelled);

    let post = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid),
    );
    if let Some(CoreEvent::RunCancelled {
        tool_children_killed,
        ..
    }) = post
        .iter()
        .find(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid))
    {
        assert!(
            *tool_children_killed >= 1,
            "toolChildrenKilled must be >= 1 when a tool child was live: {post:?}"
        );
    } else {
        panic!("no runCancelled on the wire: {post:?}");
    }

    assert!(
        wait_dead(leader_pid, Duration::from_secs(5)),
        "leader {leader_pid} still alive after cancel"
    );
    assert!(
        wait_dead(bg_pid, Duration::from_secs(5)),
        "grandchild {bg_pid} still alive — group kill missed it"
    );
    // The grandchild sleeps 1 s then touches the sentinel. We wait 3 s — enough for the
    // sentinel to appear if the grandchild survived the cancel. If the group kill works, the
    // grandchild is already dead and the sentinel never appears.
    std::thread::sleep(Duration::from_secs(3));
    assert!(
        !sentinel.exists(),
        "sentinel exists — the grandchild ran its action despite the cancel"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ── (2b) Supersede-then-cancel kills the replacement attempt's child ───────────────────────────

/// Registry clobber regression (core#500): before the fix, a superseded attempt's late `on_done`
/// deregistered the REPLACEMENT attempt's pgid (registry keyed by run_id only), so a subsequent
/// cancel emitted `toolChildrenKilled: 0` while the attempt-1 child was still alive.
///
/// This test WOULD FAIL on the pre-fix code 3/5 runs (reproduced empirically).
#[test]
fn supersede_then_cancel_kills_replacement_attempt_child() {
    let dir = fixture_dir("supersede-cancel");
    let leader0 = dir.join("leader0.pid");
    let leader1 = dir.join("leader1.pid");
    // Attempt-0 writes leader0.pid, attempt-1 writes leader1.pid.
    let sid = "toolkill-sup-cancel";
    let core = Core::spawn_with_engine(
        db_path(&dir),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();

    // Register two workflows: the first launch uses wf0, the reassign upgrades to wf1 on the
    // same unit via the dispatch, but we can't swap cmd mid-run via API. Instead: both use the
    // SAME sleep-300 script so the replacement also blocks. The key is that both attempts write
    // a different pid file to confirm which attempt is live.
    //
    // In practice the second attempt uses the same cmd as the first (reassign re-dispatches the
    // unit with the same workflow). We use a single workflow whose script first checks for
    // leader0 being absent to write leader0 (attempt 0) or writes leader1 (attempt 1). POSIX sh:
    let script = format!(
        "if [ ! -f '{l0}' ]; then echo $$ > '{l0}'; else echo $$ > '{l1}'; fi; sleep 300",
        l0 = leader0.display(),
        l1 = leader1.display(),
    );
    core.register_workflow(tool_def("sc-tool", &script))
        .unwrap();
    core.launch_run(spec(sid, "sc-tool")).expect("launch");

    // Wait for attempt-0 to be dispatched and running.
    let pre = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid),
    );
    assert!(
        pre.iter().any(
            |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid)
        ),
        "attempt-0 dispatched: {pre:?}"
    );
    let pid0 = read_pid(&leader0);
    assert!(alive(pid0), "attempt-0 alive before supersede");

    // Supersede: kills attempt-0, dispatches attempt-1.
    core.reassign_unit(sid, 1, Some("stub".to_string()))
        .expect("reassign");

    // Wait for attempt-1 to be dispatched and running.
    let post_sup = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid),
    );
    // Attempt-1 must have written leader1.pid.
    let pid1 = read_pid(&leader1);
    assert!(alive(pid1), "attempt-1 alive before cancel");
    assert_ne!(pid0, pid1, "attempt-0 and attempt-1 must be different pids");

    // Now cancel the run. With the fix, the registry holds attempt-1's pgid; cancel kills it.
    let status = core.cancel_run(sid).expect("cancel");
    assert_eq!(status, SessionStatus::Cancelled);

    let post_cancel = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid),
    );
    if let Some(CoreEvent::RunCancelled {
        tool_children_killed,
        ..
    }) = post_cancel
        .iter()
        .find(|e| matches!(e, CoreEvent::RunCancelled { session, .. } if session == sid))
    {
        assert!(
            *tool_children_killed >= 1,
            "toolChildrenKilled must be >= 1 — attempt-1's child must be in the registry at cancel; \
             if 0, the registry-clobber bug (core#500) regressed: {post_cancel:?}"
        );
    } else {
        panic!("no runCancelled on the wire: {post_cancel:?}");
    }

    assert!(
        wait_dead(pid1, Duration::from_secs(5)),
        "attempt-1 leader {pid1} still alive after cancel"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let _ = post_sup; // suppress unused warning
}

// ── (3) Supersede leaves exactly one live attempt; previousAttemptReaped on wire ──────────────

/// Design test (3) AC3: `ReassignUnit` kills the old attempt's tool child BEFORE dispatching the
/// replacement. The old process group is dead; `unitReassigned.previousAttemptReaped: true`.
#[test]
fn supersede_kills_old_attempt_and_previous_attempt_reaped_is_on_wire() {
    let dir = fixture_dir("supersede");
    let leader = dir.join("leader.pid");
    let script = format!("echo $$ > '{}'; sleep 300", leader.display());
    let sid = "toolkill-supersede";
    let core = Core::spawn_with_engine(
        db_path(&dir),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(tool_def("supersede-tool", &script))
        .unwrap();
    core.launch_run(spec(sid, "supersede-tool"))
        .expect("launch");

    let pre = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid),
    );
    assert!(
        pre.iter().any(
            |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid)
        ),
        "attempt-0 dispatched: {pre:?}"
    );
    let pid0 = read_pid(&leader);
    assert!(alive(pid0), "attempt-0 leader alive before supersede");

    core.reassign_unit(sid, 1, Some("stub".to_string()))
        .expect("reassign_unit");

    let post = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::UnitReassigned { session, .. } if session == sid),
    );
    let reassigned = post
        .iter()
        .find(|e| matches!(e, CoreEvent::UnitReassigned { session, .. } if session == sid))
        .expect("unitReassigned on the wire");
    if let CoreEvent::UnitReassigned {
        previous_attempt_reaped,
        attempt,
        ..
    } = reassigned
    {
        assert_eq!(*attempt, 1, "new attempt is 1");
        assert!(
            *previous_attempt_reaped,
            "previousAttemptReaped must be true when a tool child was killed: {post:?}"
        );
    }

    assert!(
        wait_dead(pid0, Duration::from_secs(5)),
        "attempt-0 leader {pid0} still alive after supersede"
    );

    let _ = core.cancel_run(sid);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── (4) ToolResultDiscarded fires for a superseded attempt's late Cancelled result ─────────────

/// Design test (4) AC3 (discarded late-result path): after `ReassignUnit`, the killed attempt's
/// `Cancelled` result posts back and fires `ToolResultDiscarded{reason:"superseded"}`.
#[test]
fn superseded_attempt_late_result_emits_tool_result_discarded() {
    let dir = fixture_dir("discarded");
    let leader = dir.join("leader.pid");
    let script = format!("echo $$ > '{}'; sleep 300", leader.display());
    let sid = "toolkill-discarded";
    let core = Core::spawn_with_engine(
        db_path(&dir),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(tool_def("discarded-tool", &script))
        .unwrap();
    core.launch_run(spec(sid, "discarded-tool"))
        .expect("launch");

    let pre = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid),
    );
    assert!(
        pre.iter().any(
            |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session == sid)
        ),
        "attempt-0 dispatched: {pre:?}"
    );
    let _pid = read_pid(&leader);

    core.reassign_unit(sid, 1, Some("stub".to_string()))
        .expect("reassign");

    // The superseded attempt's background thread will detect stop() and post Cancelled → Stale →
    // ToolResultDiscarded{reason:"superseded"}.
    let post = collect_until(&ev, Duration::from_secs(8), |e| {
        matches!(
            e,
            CoreEvent::ToolResultDiscarded { session, reason, .. }
                if session == sid && reason == "superseded"
        )
    });
    assert!(
        post.iter().any(|e| matches!(
            e,
            CoreEvent::ToolResultDiscarded { session, reason, .. }
                if session == sid && reason == "superseded"
        )),
        "ToolResultDiscarded{{reason:superseded}} must fire after supersede: {post:?}"
    );

    let _ = core.cancel_run(sid);
    let _ = std::fs::remove_dir_all(&dir);
}

// ── (7) Campaign cancel kills the live tool child through cancel_campaign ──────────────────────

/// Design test (7) — fix #3 coverage: `campaign::cancel` calls `cancel_run`, which now has the
/// tombstone + advance_launch_seq inside it. This test proves the full cancel path from
/// `cancel_campaign` reaches the tool child's process group: both the leader and the background
/// process are dead after `cancel_campaign`, and `runCancelled.toolChildrenKilled >= 1`.
#[test]
fn campaign_cancel_kills_the_live_tool_child_through_cancel_campaign() {
    let cid = "toolkill-campaign-cancel";
    let node_id = "node1";
    let dir = fixture_dir("campaign-cancel");
    let leader = dir.join("leader.pid");
    let bg = dir.join("bg.pid");
    let script = format!(
        "echo $$ > '{ldr}'; sleep 300 & echo $! > '{bg}'; sleep 300",
        ldr = leader.display(),
        bg = bg.display(),
    );

    let core = Core::spawn_with_engine(
        db_path(&dir),
        Arc::new(NumericDispatcher),
        Arc::new(OkRunner),
    );
    let ev = core.subscribe();
    core.register_workflow(tool_def("campaign-tool", &script))
        .unwrap();

    // The run_id under a campaign is "{cid}:{node_id}:a{attempt}" — a0 for the first dispatch.
    let run_sid = format!("{cid}:{node_id}:a0");

    let def = CampaignDef {
        id: cid.into(),
        name: "toolkill-campaign".into(),
        nodes: vec![CampaignNode {
            node_id: node_id.into(),
            run_spec: RunSpec {
                problem: "kill test".into(),
                clis: vec![cli("stub")],
                entity_mode: EntityMode::Shared,
                human_confirm: HumanConfirm::None,
                repo_ref: None,
                workflow_id: Some("campaign-tool".into()),
            },
        }],
        edges: vec![],
        policy: FailurePolicy::FailFast,
        max_concurrency: 1,
        denial_gate: Default::default(),
    };
    core.launch_campaign(def).expect("launch campaign");

    // Wait for the tool thread to be dispatched and both pids written.
    let pre = collect_until(
        &ev,
        Duration::from_secs(10),
        |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session.as_str() == run_sid),
    );
    assert!(
        pre.iter().any(
            |e| matches!(e, CoreEvent::ToolExecutorDispatched { session, .. } if session.as_str() == run_sid)
        ),
        "tool dispatched: {pre:?}"
    );
    let leader_pid = read_pid(&leader);
    let bg_pid = read_pid(&bg);
    assert!(alive(leader_pid), "leader alive before campaign cancel");
    assert!(alive(bg_pid), "grandchild alive before campaign cancel");

    let status = core.cancel_campaign(cid).expect("cancel_campaign");
    assert_eq!(status, CampaignStatus::Cancelled);

    let post = collect_until(
        &ev,
        Duration::from_secs(5),
        |e| matches!(e, CoreEvent::RunCancelled { session, .. } if session.as_str() == run_sid),
    );
    if let Some(CoreEvent::RunCancelled {
        tool_children_killed,
        ..
    }) = post.iter().find(
        |e| matches!(e, CoreEvent::RunCancelled { session, .. } if session.as_str() == run_sid),
    ) {
        assert!(
            *tool_children_killed >= 1,
            "toolChildrenKilled must be >= 1: campaign cancel must reach the tool child group: {post:?}"
        );
    } else {
        panic!("no runCancelled on the wire after campaign cancel: {post:?}");
    }

    assert!(
        wait_dead(leader_pid, Duration::from_secs(5)),
        "leader {leader_pid} still alive after campaign cancel"
    );
    assert!(
        wait_dead(bg_pid, Duration::from_secs(5)),
        "background {bg_pid} still alive — group kill missed it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
