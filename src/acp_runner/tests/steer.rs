//! DES-TEAMING-002 §8.9 (T6): the Claude ACP steer point, sourced from the bus.
//!
//! The delivery point and the request are DES-001 §5.2's, unchanged; the source is a poll of the
//! attempt's `finding.raised{severity:"high"}` rows after its own `step.claimed`, and every steer
//! is recorded as one `advice.delivered{channel:"acp_steering"}` row per finding it carried. Every
//! test drives the REAL turn loop (`exec_turn_acp_posture`, or `run_unit` end to end) against a
//! mock bridge that speaks claude-agent-acp 0.73.0's steering contract as the adapter implements
//! it (`dist/acp-agent.js`):
//!
//! * `initialize` advertises `_meta.steering.supported: true` at the TOP LEVEL of the result
//!   (`:853-859`) — or not at all, for the unadvertised bridge;
//! * the request is validated as `parseSteerRequest` does (`:131-154`): a non-empty `sessionId`, a
//!   non-empty `prompt` array, and `_meta.steering.idleBehavior` absent or `"promptRequired"`, else
//!   `-32602 Invalid params: unsupported steering idleBehavior`;
//! * with a turn in flight it answers `{"outcome":"injected"}` (`:1271`); with the turn settled it
//!   answers `{"outcome":"promptRequired","reason":"noRunningTurn"}` under the opt-in and otherwise
//!   STARTS A DETACHED TURN and answers `{"outcome":"startedNewTurn"}` (`:1228-1242`);
//! * a refused steer is the adapter's `RequestError.internalError(undefined, SESSION_ENDED_MESSAGE)`
//!   (`:1215-1217`), serialized by @agentclientprotocol/sdk 1.4.0 as `-32603 Internal error: …`.
//!
//! The bridge logs every frame it receives, so "exactly one steering frame" and "no further
//! `session/prompt`" are read off what the adapter actually got. The bus and the team outbox are a
//! temp `rig`: nothing here writes a real outbox.

use super::*;
use crate::team::events as tev;
use crate::team::publish::tests::{fixture_with, rig, Rig};
use crate::team::publish::PublishOutcome;
use crate::team::{TeamTurn, TurnClaim};

const LIVE_LINE: &str = "fetchCoverage(scope).then(setCount)";
const RUN: &str = "run-s3";

fn write_steering_bridge(dir: &std::path::Path) -> std::path::PathBuf {
    let path = dir.join("steering-bridge");
    std::fs::write(
        &path,
        r#"#!/usr/bin/env python3
import sys, json, os, select, time
behavior = sys.argv[1]
log = sys.argv[2]
# How long a turn waits at its tool-call boundary for a steer: long when the test expects one (the
# client re-snapshots the worktree first, slow on a loaded host), short when it expects none — a
# steer sent anyway is still read and logged after the turn, so it is never missed.
steer_wait = float(sys.argv[3]) if len(sys.argv) > 3 else 30.0
buf = b""

def w(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()

def logged(obj):
    with open(log, "a") as f:
        f.write(json.dumps(obj) + "\n")

def read(timeout=None):
    global buf
    while b"\n" not in buf:
        if timeout is not None:
            r, _, _ = select.select([0], [], [], timeout)
            if not r:
                return "TIMEOUT"
        chunk = os.read(0, 65536)
        if not chunk:
            return None
        buf += chunk
    line, buf = buf.split(b"\n", 1)
    try:
        obj = json.loads(line)
    except Exception:
        return read(timeout)
    logged(obj)
    return obj

def update(sid, u):
    w({"jsonrpc": "2.0", "method": "session/update", "params": {"sessionId": sid, "update": u}})

def chunk(sid, text):
    update(sid, {"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": text}})

turn_open = False
sid = "steer-session"

def steer(req):
    # parseSteerRequest (acp-agent.js:131-154), then steer() (:1210-1272).
    p = req.get("params") or {}
    meta = p.get("_meta") if isinstance(p.get("_meta"), dict) else {}
    st = meta.get("steering") if isinstance(meta.get("steering"), dict) else {}
    idle = st.get("idleBehavior")
    if not p.get("sessionId") or not isinstance(p.get("prompt"), list) or not p["prompt"]:
        w({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32602, "message": "Invalid params: steer params require a non-empty prompt array"}})
        return
    if idle is not None and idle != "promptRequired":
        w({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32602, "message": "Invalid params: unsupported steering idleBehavior"}})
        return
    if behavior == "refuse":
        w({"jsonrpc": "2.0", "id": req["id"], "error": {"code": -32603, "message": "Internal error: The Claude Agent session has ended. Please start a new session."}})
        return
    if not turn_open:
        if idle == "promptRequired":
            w({"jsonrpc": "2.0", "id": req["id"], "result": {"outcome": "promptRequired", "reason": "noRunningTurn"}})
            return
        # The default the opt-in exists to prevent: a DETACHED turn nobody asked for.
        chunk(sid, "DETACHED-TURN-OUTPUT")
        w({"jsonrpc": "2.0", "id": req["id"], "result": {"outcome": "startedNewTurn"}})
        return
    w({"jsonrpc": "2.0", "id": req["id"], "result": {"outcome": "injected"}})

def tool_call(n):
    tid = "toolu_%d" % n
    update(sid, {"sessionUpdate": "tool_call", "toolCallId": tid, "kind": "edit",
                 "title": "Edit src/retire.ts", "status": "pending",
                 "locations": [{"path": "src/retire.ts"}]})
    update(sid, {"sessionUpdate": "tool_call_update", "toolCallId": tid, "status": "completed",
                 "_meta": {"claudeCode": {"toolName": "Edit"}}})

while True:
    req = read()
    if req is None:
        break
    m = req.get("method")
    if m == "initialize":
        result = {"protocolVersion": 1, "agentCapabilities": {}, "authMethods": []}
        if behavior != "no_steer":
            result["_meta"] = {"steering": {"supported": True}}
        w({"jsonrpc": "2.0", "id": req["id"], "result": result})
    elif m == "session/new":
        w({"jsonrpc": "2.0", "id": req["id"], "result": {"sessionId": sid}})
    elif m == "session/prompt":
        pid = req["id"]
        turn_open = True
        tool_call(1)
        if behavior == "late":
            # The turn settles the instant its tool call does: the steer lands after it.
            turn_open = False
            w({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})
            continue
        if behavior == "no_steer":
            tool_call(2)
        # Wait (bounded) for a steer at this boundary.
        deadline = time.time() + steer_wait
        while time.time() < deadline:
            nxt = read(deadline - time.time())
            if nxt in (None, "TIMEOUT"):
                break
            if nxt.get("method") == "_session/steering":
                steer(nxt)
                break
        if behavior == "refuse":
            chunk(sid, "turn continued after the refusal\n")
        try:
            with open(log + ".answer") as f:
                chunk(sid, f.read())
        except FileNotFoundError:
            pass
        turn_open = False
        w({"jsonrpc": "2.0", "id": pid, "result": {"stopReason": "end_turn"}})
    elif m == "_session/steering":
        steer(req)
    elif "id" in req and m is not None:
        w({"jsonrpc": "2.0", "id": req["id"], "result": {}})
"#,
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Publish `ev`; its event id.
fn publish(rig: &Rig, ev: &tev::TeamEvent) -> i64 {
    match rig.team_bus().publish(ev).unwrap() {
        PublishOutcome::Published(id) => id,
        o => panic!("{} not published: {o:?}", ev.event_type()),
    }
}

/// A `step.claimed` row for `(3, attempt)`: the attempt's floor.
fn claim(rig: &Rig, attempt: u32) -> i64 {
    publish(
        rig,
        &fixture_with(tev::STEP_CLAIMED, 0, RUN, |p| {
            p["ord"] = serde_json::json!(3);
            p["attempt"] = serde_json::json!(attempt);
            p["by"] = serde_json::json!("claude#1");
        }),
    )
}

/// A `finding.raised` row (S) for `(3, attempt, raise_seq)` at `src/retire.ts:2`; its finding id.
fn raise(rig: &Rig, attempt: u32, raise_seq: u32, severity: &str, evidence: &str) -> String {
    let ev = fixture_with(tev::FINDING_RAISED, 0, RUN, |p| {
        p["ord"] = serde_json::json!(3);
        p["attempt"] = serde_json::json!(attempt);
        p["raise_seq"] = serde_json::json!(raise_seq);
        p["severity"] = serde_json::json!(severity);
        p["path"] = serde_json::json!("src/retire.ts");
        p["line"] = serde_json::json!(2);
        p["evidence"] = serde_json::json!(evidence);
        p["anchor"] = serde_json::Value::Null;
    });
    publish(rig, &ev);
    match ev.body {
        tev::TeamBody::FindingRaised(b) => b.finding_id,
        _ => unreachable!(),
    }
}

/// `(raise_seq, finding_id, channel, outcome, detail, steer_id)` of one `advice.delivered` row.
type Delivered = (u64, String, String, String, Option<String>, Option<String>);

/// Every `advice.delivered` row of the run, in bus order.
fn delivered(rig: &Rig) -> Vec<Delivered> {
    crate::bus::BusDb::shared(&rig.bus)
        .unwrap()
        .poll(tev::ADVICE_DELIVERED, 0, 1000)
        .unwrap()
        .into_iter()
        .filter(|e| e.payload["run_id"] == RUN)
        .map(|e| {
            let p = e.payload;
            let s = |k: &str| p[k].as_str().map(str::to_string);
            (
                p["raise_seq"].as_u64().unwrap(),
                s("finding_id").unwrap(),
                s("channel").unwrap(),
                s("outcome").unwrap(),
                s("detail"),
                s("steer_id"),
            )
        })
        .collect()
}

fn team_runner(rig: &Rig) -> crate::team::runner::TeamRunner {
    crate::team::runner::TeamRunner::from_config(
        &crate::team::publish::TeamConfig::new(Some(rig.bus.clone()), Some(rig.outbox.clone()))
            .with_attempt_wait(Duration::from_millis(30)),
    )
    .unwrap()
}

/// A worktree whose `src/retire.ts` line 2 is [`LIVE_LINE`], and its git dir.
fn worktree(dir: &std::path::Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let wt = dir.join("wt");
    std::fs::create_dir_all(wt.join("src")).unwrap();
    std::fs::write(
        wt.join("src").join("retire.ts"),
        format!("export function retire() {{\n  {LIVE_LINE}\n}}\n"),
    )
    .unwrap();
    // Through the engine's own hardened git runner (the spawn-audit chokepoint).
    crate::worktree_guard::git(&wt, &["init", "-q"], &[]).expect("git init");
    let gd = wt.join(".git");
    (wt, gd)
}

struct Turn {
    result: TurnResult,
    frames: Vec<Value>,
    steering_supported: bool,
}

impl Turn {
    fn steers(&self) -> Vec<&Value> {
        self.frames
            .iter()
            .filter(|f| f["method"] == "_session/steering")
            .collect()
    }
    fn prompts(&self) -> usize {
        self.frames
            .iter()
            .filter(|f| f["method"] == "session/prompt")
            .count()
    }
}

/// One real turn on the mock bridge through `exec_turn_acp_posture`, with `team`.
fn run_turn(
    dir: &std::path::Path,
    behavior: &str,
    team: Option<&TeamTurn>,
    answer: Option<&str>,
) -> Turn {
    run_turn_waiting(dir, behavior, team, answer, "30")
}

/// [`run_turn`] with the bridge's boundary wait (seconds) — short for tests that expect no steer.
fn run_turn_waiting(
    dir: &std::path::Path,
    behavior: &str,
    team: Option<&TeamTurn>,
    answer: Option<&str>,
    steer_wait: &str,
) -> Turn {
    let bridge = write_steering_bridge(dir);
    let log = dir.join(format!("{behavior}-frames.ndjson"));
    if let Some(a) = answer {
        std::fs::write(dir.join(format!("{behavior}-frames.ndjson.answer")), a).unwrap();
    }
    let config = AcpConfig {
        binary: bridge.to_string_lossy().to_string(),
        start_args: vec![
            behavior.to_string(),
            log.to_string_lossy().to_string(),
            steer_wait.to_string(),
        ],
        transport: AcpTransport::default(),
        auth_method: None,
        acp_input_governance: false,
        os_sandbox: false,
        acp_governance_env: None,
        verified_version: None,
    };
    let mut proc = {
        let _env = ENV_LOCK.read().unwrap_or_else(|p| p.into_inner());
        start_acp_process(&config, dir, None, None).expect("mock steering bridge starts")
    };
    let steering_supported = proc.steering_supported;
    let (tx, _rx) = std::sync::mpsc::channel();
    let noop: &DeltaSink = &|_: &str| {};
    let result = exec_turn_acp_posture(
        &mut proc,
        "do the work",
        &[],
        noop,
        Duration::from_secs(30),
        Arc::new(Mutex::new(ElicitationMaps::new())),
        RUN,
        0,
        &tx,
        None,
        None,
        team,
    )
    .expect("the turn completes");
    // Anything the client would write after the turn (a second prompt, a retry) lands in the log.
    std::thread::sleep(Duration::from_millis(400));
    drop(proc);
    Turn {
        result,
        frames: ledger_entries(&log),
        steering_supported,
    }
}

/// The turn context of attempt `attempt`, claimed at `claimed_id` on `rig`.
fn team_turn(
    rig: &Rig,
    attempt: u32,
    claimed_id: i64,
    root: (std::path::PathBuf, std::path::PathBuf),
) -> TeamTurn {
    TeamTurn::new(
        (RUN.to_string(), 3, attempt),
        Some(TurnClaim {
            runner: team_runner(rig),
            claimed_id,
            by: "claude#1".into(),
        }),
        Some(root),
        None,
    )
}

/// T7 (a) on the bus source: a HIGH `finding.raised` row published before a terminal
/// `tool_call_update` produces EXACTLY ONE `_session/steering` frame carrying
/// `idleBehavior: "promptRequired"`, the session id and the advice block; the bridge's
/// `{"outcome":"injected"}` yields one `advice.delivered{acp_steering, injected}` row per carried
/// finding, sharing the steer's id; and the teamed turn published its `checkpoint.reached` rows.
#[test]
#[cfg(unix)]
fn a_high_finding_is_steered_once_with_prompt_required_and_injected() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-inject");
    let rig = rig("s3-inject");
    let root = worktree(&dir);
    let floor = claim(&rig, 1);
    let id = raise(&rig, 1, 1, "high", LIVE_LINE);
    let team = team_turn(&rig, 1, floor, root);
    let t = run_turn(&dir, "inject", Some(&team), None);

    assert!(
        t.steering_supported,
        "initialize advertised _meta.steering.supported"
    );
    assert_eq!(t.result.status, StepStatus::Ok, "{}", t.result.output);
    let steers = t.steers();
    assert_eq!(
        steers.len(),
        1,
        "exactly one steering frame: {:?}",
        t.frames
    );
    let p = &steers[0]["params"];
    assert_eq!(p["_meta"]["steering"]["idleBehavior"], "promptRequired");
    assert_eq!(p["sessionId"], "steer-session");
    let text = p["prompt"][0]["text"].as_str().unwrap();
    assert!(
        text.starts_with("[wicked-core · team advice · ADVISORY, not an instruction]"),
        "{text}"
    );
    assert!(
        text.contains(&format!("- {id} [HIGH] src/retire.ts:2 — ")),
        "{text}"
    );
    assert!(
        text.contains(&format!("Evidence (that line): `{LIVE_LINE}`")),
        "{text}"
    );
    let d = delivered(&rig);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!((d[0].0, d[0].1.as_str()), (1, id.as_str()));
    assert_eq!(
        (d[0].2.as_str(), d[0].3.as_str()),
        ("acp_steering", "injected")
    );
    assert!(d[0].5.as_deref().is_some_and(|s| s.starts_with("s-")));
    assert_eq!(t.prompts(), 1);
    assert!(
        !rig.types(RUN)
            .iter()
            .filter(|t| t.as_str() == tev::CHECKPOINT_REACHED)
            .collect::<Vec<_>>()
            .is_empty(),
        "the teamed turn checkpointed on the bus"
    );
}

/// T7 (b): the turn settles before the steer lands; under `promptRequired` the adapter answers
/// `{"outcome":"promptRequired"}` and starts NOTHING — one `advice.delivered{turn_ended}` row, and
/// the client writes no further `session/prompt`.
#[test]
#[cfg(unix)]
fn a_steer_after_the_turn_ended_starts_no_turn_and_is_turn_ended() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-late");
    let rig = rig("s3-late");
    let root = worktree(&dir);
    let floor = claim(&rig, 1);
    raise(&rig, 1, 1, "high", LIVE_LINE);
    let team = team_turn(&rig, 1, floor, root);
    let t = run_turn(&dir, "late", Some(&team), None);

    assert_eq!(t.result.status, StepStatus::Ok, "{}", t.result.output);
    assert_eq!(t.steers().len(), 1, "{:?}", t.frames);
    let d = delivered(&rig);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].3, "turn_ended", "{d:?}");
    assert_eq!(d[0].4.as_deref(), Some("noRunningTurn"));
    assert_eq!(t.prompts(), 1, "no further session/prompt: {:?}", t.frames);
    assert!(!t.result.output.contains("DETACHED-TURN-OUTPUT"));
}

/// T7 (c): a bridge that does not advertise steering receives ZERO `_session/steering` frames, and
/// the carrier publishes no delivery row: the finding reaches the PA at the next step boundary
/// (T5), and the gate reads it as not delivered.
#[test]
#[cfg(unix)]
fn an_unadvertised_bridge_gets_no_steer_and_publishes_no_delivery() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-nosteer");
    let rig = rig("s3-nosteer");
    let root = worktree(&dir);
    let floor = claim(&rig, 1);
    raise(&rig, 1, 1, "high", LIVE_LINE);
    let team = team_turn(&rig, 1, floor, root);
    let t = run_turn_waiting(&dir, "no_steer", Some(&team), None, "2");

    assert!(!t.steering_supported);
    assert_eq!(t.result.status, StepStatus::Ok, "{}", t.result.output);
    assert!(t.steers().is_empty(), "{:?}", t.frames);
    assert!(delivered(&rig).is_empty());
}

/// T7 (d): a MEDIUM row is never steered, and a HIGH whose evidence text is gone from the fresh
/// snapshot is not sent (the supervisor supersedes it) — while a HIGH still present is.
#[test]
#[cfg(unix)]
fn medium_is_never_steered_and_gone_evidence_is_not_sent() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-supersede");
    let rig = rig("s3-supersede");
    let root = worktree(&dir);
    let floor = claim(&rig, 1);
    let medium = raise(&rig, 1, 1, "medium", "export function retire() {");
    let live = raise(&rig, 1, 2, "high", LIVE_LINE);
    let gone = raise(&rig, 1, 3, "high", "this line was rewritten away");
    let team = team_turn(&rig, 1, floor, root);
    let t = run_turn(&dir, "inject", Some(&team), None);

    let steers = t.steers();
    assert_eq!(steers.len(), 1, "{:?}", t.frames);
    let text = steers[0]["params"]["prompt"][0]["text"].as_str().unwrap();
    assert!(text.contains(&live), "{text}");
    assert!(!text.contains(&gone), "a gone finding is not sent: {text}");
    assert!(!text.contains(&medium), "{text}");
    let d = delivered(&rig);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].1, live);
}

/// T7 (e): a row for attempt 1 never reaches attempt 2 of the same unit.
#[test]
#[cfg(unix)]
fn advice_for_attempt_one_never_reaches_attempt_two() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-attempt");
    let rig = rig("s3-attempt");
    let root = worktree(&dir);
    claim(&rig, 1);
    raise(&rig, 1, 1, "high", LIVE_LINE);
    let floor2 = claim(&rig, 2);
    let team = team_turn(&rig, 2, floor2, root);
    let t = run_turn_waiting(&dir, "inject", Some(&team), None, "2");

    assert!(t.steers().is_empty(), "{:?}", t.frames);
    assert!(delivered(&rig).is_empty());
}

/// A JSON-RPC error answer to the steer yields one `advice.delivered{refused, detail}` row with the
/// adapter's error, and the turn CONTINUES to its normal end.
#[test]
#[cfg(unix)]
fn a_refused_steer_is_disclosed_and_the_turn_continues() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-refuse");
    let rig = rig("s3-refuse");
    let root = worktree(&dir);
    let floor = claim(&rig, 1);
    raise(&rig, 1, 1, "high", LIVE_LINE);
    let team = team_turn(&rig, 1, floor, root);
    let t = run_turn(&dir, "refuse", Some(&team), None);

    assert_eq!(t.result.status, StepStatus::Ok, "{}", t.result.output);
    assert!(t.result.output.contains("turn continued after the refusal"));
    let d = delivered(&rig);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].3, "refused");
    assert!(
        d[0].4
            .as_deref()
            .unwrap()
            .starts_with("-32603: Internal error"),
        "{d:?}"
    );
}

/// A turn without a team context — or one whose attempt is not claimed — never steers and never
/// checkpoints.
#[test]
#[cfg(unix)]
fn a_turn_without_a_claim_never_steers() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-noteam");
    let t = run_turn_waiting(&dir, "inject", None, None, "2");
    assert!(t.steers().is_empty());
    let rig = rig("s3-noclaim");
    let root = worktree(&dir);
    claim(&rig, 1);
    raise(&rig, 1, 1, "high", LIVE_LINE);
    let unclaimed = TeamTurn::new((RUN.to_string(), 3, 1), None, Some(root), None);
    let t = run_turn_waiting(&dir, "inject", Some(&unclaimed), None, "2");
    assert!(t.steers().is_empty());
    assert!(delivered(&rig).is_empty());
    assert!(!rig
        .types(RUN)
        .iter()
        .any(|t| t.as_str() == tev::CHECKPOINT_REACHED));
}

/// Seat overlay: `seat-key` over `bridge_args` on `[cli.acp]`.
fn seat_overlay(home: &std::path::Path, bridge: (&std::path::Path, &[&str])) {
    let council = home.join(".config").join("wicked-council");
    std::fs::create_dir_all(&council).unwrap();
    let (b, args) = bridge;
    let acp = format!(
        "\n[cli.acp]\nbinary = \"{}\"\nstart_args = [{}]\ntransport = \"stdio\"\n",
        b.display(),
        args.iter()
            .map(|a| format!("\"{a}\""))
            .collect::<Vec<_>>()
            .join(", ")
    );
    std::fs::write(
        council.join("clis.toml"),
        format!(
            "[[cli]]\nkey = \"steer-seat\"\ndisplay_name = \"Steer seat\"\nbinary = \"s3-no-such-cli\"\n\
             headless_invocation = \"s3-no-such-cli -p \\\"{{PROMPT}}\\\"\"\n{acp}"
        ),
    )
    .unwrap();
}

/// End to end through the PRODUCTION seam (`AcpStepRunner::run_unit` with its installed team
/// runner): a unit whose snapshot says its attempt is claimed on the bus gets the HIGH row steered
/// and one `advice.delivered{injected}` row; the worker's `ADVICE` lines are the worker thread's
/// to read (`team::runner::complete`), not the carrier's.
#[test]
#[cfg(unix)]
fn run_unit_steers_a_claimed_attempt_from_the_bus() {
    use crate::workflow::StepRunner;
    let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
    let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
    let home = crate::skills_snapshot::test_support::scratch("s3-e2e");
    let _home = EnvPin::set("HOME", &home);
    let rig = rig("s3-e2e");
    let (wt, _) = worktree(&home);
    let bridge = write_steering_bridge(&home);
    let log = home.join("e2e-frames.ndjson");
    seat_overlay(&home, (&bridge, &["inject", &log.to_string_lossy()]));
    let floor = claim(&rig, 1);
    let id = raise(&rig, 1, 1, "high", LIVE_LINE);
    let (tx, _rx) = std::sync::mpsc::channel();
    let runner = AcpStepRunner::new(tx);
    runner.install_team_runner(team_runner(&rig));

    let mut u = crate::domain::WorkUnit::pending("run-s3:u3", RUN, 3, "do the thing");
    u.assigned_cli = Some("steer-seat".to_string());
    u.team_run = true;
    let mut snap = crate::domain::UnitTeamSnapshot::stamped(tev::Transport::Bus, None, None);
    snap.stream_floor = Some(floor);
    snap.claimed_event_id = Some(floor);
    u.team = Some(snap);
    u.worktree_baseline = Some(crate::worktree_guard::WorktreeSnapshot {
        head: String::new(),
        head_ref: None,
        tree: String::new(),
        taken_at_ms: 0,
        git_dir: Some(wt.join(".git").to_string_lossy().into_owned()),
    });
    let input = crate::workflow::StepInput {
        run_id: RUN.to_string(),
        unit_ix: 0,
        attempt: 1,
        unit: u,
        workflow_id: "wf-s3".to_string(),
        entity_mode: crate::scope::EntityMode::Isolated,
        workdir: Some(wt.to_path_buf()),
        governance: None,
        prior_outputs: vec![],
        elicitation_epoch: 0,
        process_gen: None,
        launch_seq: 0,
        required_skills: Vec::new(),
    };
    let out = runner.run_unit(&input);
    assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
    runner.on_run_complete(RUN);
    let steers: Vec<Value> = ledger_entries(&log)
        .into_iter()
        .filter(|f| f["method"] == "_session/steering")
        .collect();
    assert_eq!(steers.len(), 1);
    let d = delivered(&rig);
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(
        (d[0].1.as_str(), d[0].3.as_str()),
        (id.as_str(), "injected")
    );
}
