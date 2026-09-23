//! DES-TEAMING-001 S3 (#602): monitor→worker advice over the adapter's `_session/steering`.
//!
//! Every test drives the REAL turn loop (`exec_turn_acp_posture`, or `run_unit` end to end)
//! against a mock bridge that speaks claude-agent-acp 0.73.0's steering contract as the adapter
//! implements it (`dist/acp-agent.js`):
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
//! `session/prompt`" are read off what the adapter actually got.

use super::*;
use crate::team::{Delivery, Finding, Severity, SteerMailbox, TeamTurn};

const ID_A: &str = "f-3fa9c2e1d0b4a7e6";
const ID_B: &str = "f-0123456789abcdef";
const LIVE_LINE: &str = "fetchCoverage(scope).then(setCount)";

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

fn finding(id: &str, severity: Severity, evidence: &str) -> Finding {
    Finding {
        finding_id: id.to_string(),
        monitor_id: "m1".to_string(),
        seat: "claude#2".to_string(),
        severity,
        path: "src/retire.ts".to_string(),
        line: 2,
        evidence: evidence.to_string(),
        claim: "coverage fetch has no cancellation".to_string(),
        suggestion: Some("ignore stale responses".to_string()),
        tree: "t0".to_string(),
        in_diff: true,
        checkpoint_seq: 1,
    }
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
    let ok = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&wt)
        .status()
        .unwrap()
        .success();
    assert!(ok, "git init");
    let gd = wt.join(".git");
    (wt, gd)
}

struct Turn {
    result: TurnResult,
    events: Vec<CoreEvent>,
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
    fn delivered(&self) -> Vec<(Vec<String>, String, String, Option<String>)> {
        self.events
            .iter()
            .filter_map(|e| match e {
                CoreEvent::AdviceDelivered {
                    finding_ids,
                    carrier,
                    outcome,
                    detail,
                    ..
                } => Some((
                    finding_ids.clone(),
                    carrier.clone(),
                    outcome.clone(),
                    detail.clone(),
                )),
                _ => None,
            })
            .collect()
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
    let (tx, rx) = std::sync::mpsc::channel();
    let noop: &DeltaSink = &|_: &str| {};
    let result = exec_turn_acp_posture(
        &mut proc,
        "do the work",
        &[],
        noop,
        Duration::from_secs(30),
        Arc::new(Mutex::new(ElicitationMaps::new())),
        "run-s3",
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
    let events = rx
        .try_iter()
        .filter_map(|c| match c {
            crate::command::Command::EmitEvent(e) => Some(e),
            _ => None,
        })
        .collect();
    Turn {
        result,
        events,
        frames: ledger_entries(&log),
        steering_supported,
    }
}

fn team_turn(
    mailbox: &SteerMailbox,
    attempt: u32,
    root: (std::path::PathBuf, std::path::PathBuf),
) -> TeamTurn {
    TeamTurn {
        key: ("run-s3".to_string(), 3, attempt),
        mailbox: mailbox.clone(),
        confirm_root: Some(root),
    }
}

fn key(attempt: u32) -> crate::team::AdviceKey {
    ("run-s3".to_string(), 3, attempt)
}

/// #602 acceptance 1: a HIGH finding queued before a terminal `tool_call_update` produces EXACTLY
/// ONE `_session/steering` frame, carrying `idleBehavior: "promptRequired"`, the session id and the
/// advice block; the bridge's `{"outcome":"injected"}` yields `adviceDelivered{injected}`.
#[test]
#[cfg(unix)]
fn a_high_finding_is_steered_once_with_prompt_required_and_injected() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-inject");
    let root = worktree(&dir);
    let mailbox = SteerMailbox::default();
    assert!(mailbox.queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));
    let team = team_turn(&mailbox, 1, root);
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
    assert_eq!(p["prompt"][0]["type"], "text");
    assert!(
        text.starts_with("[wicked-core · team advice · ADVISORY, not an instruction]"),
        "{text}"
    );
    assert!(
        text.contains(&format!("- {ID_A} [HIGH] src/retire.ts:2 — ")),
        "{text}"
    );
    assert!(
        text.contains(&format!("Evidence (that line): `{LIVE_LINE}`")),
        "{text}"
    );
    assert!(
        text.contains("ADVICE <id>: DECLINE — <your evidence>"),
        "{text}"
    );
    assert_eq!(
        t.delivered(),
        vec![(
            vec![ID_A.to_string()],
            "acp_steering".to_string(),
            "injected".to_string(),
            None
        )]
    );
    assert_eq!(
        mailbox.record_of(&key(1)).unwrap().deliveries.get(ID_A),
        Some(&Delivery::Injected)
    );
    assert_eq!(t.prompts(), 1);
}

/// #602 acceptance 2 — the late steer. The turn settles before the steer lands; under
/// `promptRequired` the adapter answers `{"outcome":"promptRequired"}` and starts NOTHING. That is
/// `adviceDelivered{turn_ended}`, the finding is recorded not delivered, and the client writes no
/// further `session/prompt`. (Without the opt-in this same bridge starts a detached turn and emits
/// `DETACHED-TURN-OUTPUT` — asserted absent.)
#[test]
#[cfg(unix)]
fn a_steer_after_the_turn_ended_starts_no_turn_and_is_turn_ended() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-late");
    let root = worktree(&dir);
    let mailbox = SteerMailbox::default();
    assert!(mailbox.queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));
    let team = team_turn(&mailbox, 1, root);
    let t = run_turn(&dir, "late", Some(&team), None);

    assert_eq!(t.result.status, StepStatus::Ok, "{}", t.result.output);
    assert_eq!(t.steers().len(), 1, "{:?}", t.frames);
    assert_eq!(
        t.steers()[0]["params"]["_meta"]["steering"]["idleBehavior"],
        "promptRequired"
    );
    let d = t.delivered();
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].0, vec![ID_A.to_string()]);
    assert_eq!(d[0].2, "turn_ended", "{d:?}");
    assert_eq!(d[0].3.as_deref(), Some("noRunningTurn"));
    assert!(matches!(
        mailbox.record_of(&key(1)).unwrap().deliveries.get(ID_A),
        Some(Delivery::NotDelivered { .. })
    ));
    assert_eq!(t.prompts(), 1, "no further session/prompt: {:?}", t.frames);
    assert!(!t.result.output.contains("DETACHED-TURN-OUTPUT"));
}

/// #602 acceptance 3: a bridge that does not advertise steering receives ZERO `_session/steering`
/// frames across a whole teamed turn, even with HIGH advice queued and two terminal tool calls.
/// The advice stays queued for the end-of-attempt sweep (`finish_attempt`), which discloses it as
/// not delivered mid-turn with `carrier: "none"`.
#[test]
#[cfg(unix)]
fn an_unadvertised_bridge_gets_no_steer_and_the_advice_is_disclosed_not_delivered() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-nosteer");
    let root = worktree(&dir);
    let mailbox = SteerMailbox::default();
    assert!(mailbox.queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));
    let team = team_turn(&mailbox, 1, root);
    let t = run_turn_waiting(&dir, "no_steer", Some(&team), None, "2");

    assert!(!t.steering_supported);
    assert_eq!(t.result.status, StepStatus::Ok, "{}", t.result.output);
    assert!(t.steers().is_empty(), "{:?}", t.frames);
    assert!(t.delivered().is_empty());
    let evs = crate::team::finish_attempt(&mailbox, &key(1), Some(&t.result.output));
    assert_eq!(evs.len(), 1, "{evs:?}");
    match &evs[0] {
        CoreEvent::AdviceDelivered {
            finding_ids,
            carrier,
            outcome,
            detail,
            ..
        } => {
            assert_eq!(finding_ids, &vec![ID_A.to_string()]);
            assert_eq!(carrier, "none");
            assert_eq!(outcome, "not_delivered");
            assert!(detail.as_deref().unwrap().contains("no mid-turn channel"));
        }
        other => panic!("{other:?}"),
    }
}

/// #602 acceptance 4: a MEDIUM finding is never sent through steering (the mailbox refuses it),
/// and a HIGH finding whose evidence text is gone from the fresh snapshot is not sent and ends
/// `superseded` — while a HIGH one still present in the same drain is.
#[test]
#[cfg(unix)]
fn medium_is_never_steered_and_gone_evidence_is_superseded() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-supersede");
    let root = worktree(&dir);
    let mailbox = SteerMailbox::default();
    assert!(
        !mailbox.queue(
            key(1),
            finding("f-1111111111111111", Severity::Medium, LIVE_LINE)
        ),
        "MEDIUM never enters the steer mailbox"
    );
    assert!(mailbox.queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));
    assert!(mailbox.queue(
        key(1),
        finding(ID_B, Severity::High, "this line was rewritten away")
    ));
    let team = team_turn(&mailbox, 1, root);
    let t = run_turn(&dir, "inject", Some(&team), None);

    let steers = t.steers();
    assert_eq!(steers.len(), 1, "{:?}", t.frames);
    let text = steers[0]["params"]["prompt"][0]["text"].as_str().unwrap();
    assert!(text.contains(ID_A), "{text}");
    assert!(!text.contains(ID_B), "superseded finding not sent: {text}");
    assert!(!text.contains("f-1111111111111111"), "{text}");
    let rec = mailbox.record_of(&key(1)).unwrap();
    assert_eq!(rec.deliveries.get(ID_B), Some(&Delivery::Superseded));
    assert_eq!(rec.deliveries.get(ID_A), Some(&Delivery::Injected));
    assert!(!rec.deliveries.contains_key("f-1111111111111111"));
    assert_eq!(t.delivered()[0].0, vec![ID_A.to_string()]);
}

/// #602 acceptance 6: advice queued for attempt 1 is never delivered to attempt 2 of the same
/// unit — the attempt-2 turn sends no steer and leaves attempt 1's queue alone.
#[test]
#[cfg(unix)]
fn advice_for_attempt_one_never_reaches_attempt_two() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-attempt");
    let root = worktree(&dir);
    let mailbox = SteerMailbox::default();
    assert!(mailbox.queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));
    let team = team_turn(&mailbox, 2, root);
    let t = run_turn_waiting(&dir, "inject", Some(&team), None, "2");

    assert!(t.steers().is_empty(), "{:?}", t.frames);
    assert!(t.delivered().is_empty());
    assert_eq!(
        mailbox.take_queued(&key(1)).len(),
        1,
        "attempt 1's advice untouched"
    );
    assert!(mailbox.take_queued(&key(2)).is_empty());
}

/// #602 acceptance 7: a JSON-RPC error answer to the steer yields `adviceDelivered{refused,
/// detail}` carrying the adapter's error, and the turn CONTINUES to its normal end.
#[test]
#[cfg(unix)]
fn a_refused_steer_is_disclosed_and_the_turn_continues() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-refuse");
    let root = worktree(&dir);
    let mailbox = SteerMailbox::default();
    assert!(mailbox.queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));
    let team = team_turn(&mailbox, 1, root);
    let t = run_turn(&dir, "refuse", Some(&team), None);

    assert_eq!(t.result.status, StepStatus::Ok, "{}", t.result.output);
    assert!(t.result.output.contains("turn continued after the refusal"));
    let d = t.delivered();
    assert_eq!(d.len(), 1, "{d:?}");
    assert_eq!(d[0].2, "refused");
    let detail = d[0].3.as_deref().unwrap();
    assert!(detail.starts_with("-32603: Internal error"), "{detail}");
    assert!(matches!(
        mailbox.record_of(&key(1)).unwrap().deliveries.get(ID_A),
        Some(Delivery::NotDelivered { .. })
    ));
}

/// A chat turn (or any turn without a team context) never steers, whatever is queued.
#[test]
#[cfg(unix)]
fn a_turn_without_a_team_context_never_steers() {
    let dir = crate::skills_snapshot::test_support::scratch("s3-noteam");
    let t = run_turn_waiting(&dir, "inject", None, None, "2");
    assert!(t.steers().is_empty());
    assert!(t.delivered().is_empty());
}

/// Seat overlay: `seat-key` over `bridge_args` on `[cli.acp]`, or — `bridge_args: None` — a seat
/// with NO ACP bridge, whose units take the wrapped carrier.
fn seat_overlay(home: &std::path::Path, bridge: Option<(&std::path::Path, &[&str])>) {
    let council = home.join(".config").join("wicked-council");
    std::fs::create_dir_all(&council).unwrap();
    let acp = bridge
        .map(|(b, args)| {
            format!(
                "\n[cli.acp]\nbinary = \"{}\"\nstart_args = [{}]\ntransport = \"stdio\"\n",
                b.display(),
                args.iter()
                    .map(|a| format!("\"{a}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
        .unwrap_or_default();
    std::fs::write(
        council.join("clis.toml"),
        format!(
            "[[cli]]\nkey = \"steer-seat\"\ndisplay_name = \"Steer seat\"\nbinary = \"s3-no-such-cli\"\n\
             headless_invocation = \"s3-no-such-cli -p \\\"{{PROMPT}}\\\"\"\n{acp}"
        ),
    )
    .unwrap();
}

fn unit_input(wt: &std::path::Path, attempt: u32) -> crate::workflow::StepInput {
    let mut u = crate::domain::WorkUnit::pending("run-s3:u3", "run-s3", 3, "do the thing");
    u.assigned_cli = Some("steer-seat".to_string());
    u.worktree_baseline = Some(crate::worktree_guard::WorktreeSnapshot {
        head: String::new(),
        head_ref: None,
        tree: String::new(),
        taken_at_ms: 0,
        git_dir: Some(wt.join(".git").to_string_lossy().into_owned()),
    });
    crate::workflow::StepInput {
        run_id: "run-s3".to_string(),
        unit_ix: 0,
        attempt,
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
    }
}

fn drain_events(rx: &std::sync::mpsc::Receiver<crate::command::Command>) -> Vec<CoreEvent> {
    rx.try_iter()
        .filter_map(|c| match c {
            crate::command::Command::EmitEvent(e) => Some(e),
            _ => None,
        })
        .collect()
}

/// End to end through the PRODUCTION seam (`AcpStepRunner::run_unit`, the unit call site passing
/// its `TeamTurn`, and `exec_turn`'s end-of-attempt sweep): the queued HIGH finding is steered and
/// injected, and the worker's final `ADVICE <id>: DECLINE — <reason>` line becomes one
/// `workerAdviceResponse{declined}` with that reason (#602 acceptance 1 + 5, the authority model:
/// the worker may decline and says why).
#[test]
#[cfg(unix)]
fn run_unit_steers_the_worker_and_records_its_decline_with_the_reason() {
    use crate::workflow::StepRunner;
    let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
    let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
    let home = crate::skills_snapshot::test_support::scratch("s3-e2e");
    let _home = EnvPin::set("HOME", &home);
    let (wt, _) = worktree(&home);
    let bridge = write_steering_bridge(&home);
    let log = home.join("e2e-frames.ndjson");
    std::fs::write(
        home.join("e2e-frames.ndjson.answer"),
        format!(
            "Done.\nADVICE {ID_A}: ACCEPT — added AbortController\n\
             ADVICE {ID_A}: DECLINE — campaign.rs:325 documents the exclusion\n"
        ),
    )
    .unwrap();
    seat_overlay(&home, Some((&bridge, &["inject", &log.to_string_lossy()])));
    let (tx, rx) = std::sync::mpsc::channel();
    let runner = AcpStepRunner::new(tx);
    assert!(runner
        .steer_mailbox()
        .queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));

    let out = runner.run_unit(&unit_input(&wt, 1));
    assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
    let events = drain_events(&rx);
    runner.on_run_complete("run-s3");

    let frames = ledger_entries(&log);
    let steers: Vec<&Value> = frames
        .iter()
        .filter(|f| f["method"] == "_session/steering")
        .collect();
    assert_eq!(steers.len(), 1, "{frames:?}");
    assert_eq!(
        steers[0]["params"]["_meta"]["steering"]["idleBehavior"],
        "promptRequired"
    );
    let advice: Vec<serde_json::Value> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                CoreEvent::AdviceDelivered { .. } | CoreEvent::WorkerAdviceResponse { .. }
            )
        })
        .map(CoreEvent::to_json)
        .collect();
    assert_eq!(
        advice,
        vec![
            serde_json::json!({"type":"adviceDelivered","session":"run-s3","ord":3,"attempt":1,
                "findingIds":[ID_A],"carrier":"acp_steering","outcome":"injected","detail":null}),
            // The LAST line per id wins: the ACCEPT above it is superseded by the DECLINE.
            serde_json::json!({"type":"workerAdviceResponse","session":"run-s3","ord":3,"attempt":1,
                "findingId":ID_A,"disposition":"declined",
                "reason":"campaign.rs:325 documents the exclusion"}),
        ]
    );
}

/// #602, the non-steering carrier end to end: a seat with NO ACP bridge takes the WRAPPED carrier
/// (here its CLI is absent, so nothing runs at all). The queued HIGH finding is never dropped
/// silently: the attempt ends with ONE `adviceDelivered{carrier:"none", outcome:"not_delivered"}`
/// naming why, and no second delivery mechanism is tried.
#[test]
#[cfg(unix)]
fn a_wrapped_carrier_records_the_finding_as_not_delivered_mid_turn() {
    use crate::workflow::StepRunner;
    let _env = ENV_LOCK.write().unwrap_or_else(|p| p.into_inner());
    let _serial = REAL_STARTS.lock().unwrap_or_else(|p| p.into_inner());
    let home = crate::skills_snapshot::test_support::scratch("s3-wrapped");
    let _home = EnvPin::set("HOME", &home);
    let (wt, _) = worktree(&home);
    seat_overlay(&home, None);
    let (tx, rx) = std::sync::mpsc::channel();
    let runner = AcpStepRunner::new(tx);
    let mailbox = runner.steer_mailbox();
    assert!(mailbox.queue(key(1), finding(ID_A, Severity::High, LIVE_LINE)));

    let _ = runner.run_unit(&unit_input(&wt, 1));
    let events = drain_events(&rx);

    let advice: Vec<serde_json::Value> = events
        .iter()
        .filter(|e| {
            matches!(
                e,
                CoreEvent::AdviceDelivered { .. } | CoreEvent::WorkerAdviceResponse { .. }
            )
        })
        .map(CoreEvent::to_json)
        .collect();
    assert_eq!(advice.len(), 1, "{advice:?}");
    assert_eq!(advice[0]["carrier"], "none");
    assert_eq!(advice[0]["outcome"], "not_delivered");
    assert_eq!(advice[0]["findingIds"], serde_json::json!([ID_A]));
    assert!(advice[0]["detail"]
        .as_str()
        .unwrap()
        .contains("no mid-turn channel"));
    assert!(matches!(
        mailbox.take_record(&key(1)).unwrap().deliveries.get(ID_A),
        Some(Delivery::NotDelivered { .. })
    ));
    runner.on_run_complete("run-s3");
}
