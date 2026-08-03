//! ACP (Agent Client Protocol) session runner — multi-CLI extension of wicked-core#13.
//!
//! Drives persistent multi-turn sessions using the standardised JSON-RPC 2.0 ndjson
//! (stdin/stdout) ACP protocol. Each CLI runs its own ACP server — a wrapper binary
//! or the CLI's native ACP mode; wicked-core is the ACP client. The registry maps CLI
//! keys to their ACP invocation:
//!
//! | CLI      | ACP binary / invocation                              | Transport |
//! |----------|------------------------------------------------------|-----------|
//! | claude   | claude-agent-acp (@agentclientprotocol, Agent SDK)   | stdio     |
//! | codex    | codex-acp (@agentclientprotocol, Rust)               | stdio     |
//! | pi       | pi-acp (community adapter)                           | stdio     |
//! | agy      | agy-acp (wicked-crew packages/agent-acp-bridges)     | stdio     |
//! | copilot  | copilot --acp (native)                               | stdio     |
//! | opencode | opencode acp (native)                                | stdio     |
//!
//! When an ACP binary is unavailable or fails during the handshake, `AcpStepRunner`
//! emits a warning and prepends it to `StepOutput.output` so it is visible in both
//! streaming and persisted contexts. The run then continues with single-shot fallback.
//! HTTP transport is not yet implemented (no registry entry uses it today).
//!
//! # Session lifecycle
//! - **Open (lazy)**: on the first unit for a `(run_id, cli_key)` pair, the binary is
//!   spawned and the `initialize` + `session/new` JSON-RPC handshake completes.
//! - **Reuse**: subsequent units send `session/prompt` to the same process and stream
//!   `session/update` text chunks until `stopReason` arrives — sharing prompt-cache
//!   across governance turns without a per-unit cold start.
//! - **Close**: [`AcpStepRunner::drop_session`] kills all CLI processes for a `run_id`.
//!   Call it after the last unit of a run (mirrors [`PersistentStepRunner::drop_session`]).
//!
//! # Protocol
//! JSON-RPC 2.0 ndjson over stdin/stdout. Non-JSON startup banners and log lines
//! are silently skipped during both handshake and turn execution.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::command::Command;
use crate::event::CoreEvent;
use crate::execute_wrapped::{unit_prompt, WrappedCliStepRunner};
use crate::workflow::{
    DeltaSink, PriorUnitOutput, StepInput, StepOutput, StepRunner, StepStatus, Usage,
};
use wicked_council::types::{AcpConfig, AcpTransport};

// ── ACP child process ─────────────────────────────────────────────────────────

struct AcpProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    /// Lines arriving from the ACP server's stdout, fed by the reader thread.
    /// Unbounded so the reader never blocks the child on a full pipe.
    line_rx: std::sync::mpsc::Receiver<String>,
    _reader: std::thread::JoinHandle<()>,
    /// Bounded tail of the bridge's stderr, kept for the life of the session so a turn-level
    /// failure can report what the bridge said rather than only that it stopped answering.
    stderr_tail: StderrTail,
    _stderr_reader: Option<std::thread::JoinHandle<()>>,
    session_id: String,
    next_id: u64,
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Handshake budgets and start concurrency ───────────────────────────────────
//
// The two handshake calls are budgeted SEPARATELY because they have different cost profiles.
// Sweeping bridge-startup concurrency directly (K bridges released simultaneously on a barrier,
// same host, same binary):
//
//   K   initialize (id=1)   session/new (id=2) med / max   trips a 10s budget
//   1   0.31 – 0.69s        1.67s  /  1.67s                0/1
//   2   0.31 – 0.69s        1.74s  /  1.75s                0/2
//   4   0.31 – 0.69s        3.41s  /  5.31s                0/4
//   8   0.31 – 0.69s        7.12s  / 11.57s                2/8
//
// `initialize` is flat at every K; `session/new` scales ~linearly past K=2. A single constant
// applied to both under-budgets exactly the call that contends — and every ACP timeout observed
// in the field names id=2. Host load does NOT predict this: K=1 returned 1.67s at load average
// 37.86, the highest load and the fastest sample in the same experiment.
//
// This is a governance defect, not a latency one. A handshake timeout does not fail the unit; it
// silently downgrades it to the single-shot wrapped-CLI path. So under-budgeting trades the
// governed execution path for latency without saying so, and does it most often exactly when
// parallelism is highest — which is when governance matters most. (FINDING-022)

/// Budget for `initialize`. Generous relative to the 0.69s worst case above because the FIRST
/// spawn after daemon start is cold and was measured at 9.83s.
const INIT_DEFAULT_SECS: u64 = 60;

/// Budget for `session/new` — the call whose cost scales with concurrency. ~7x the median measured
/// in the slow regime and ~5x its worst case.
const SESSION_NEW_DEFAULT_SECS: u64 = 60;

/// How many ACP bridges may be inside `initialize` + `session/new` at once. 2 is the highest
/// concurrency measured with no degradation. What is bounded is contention, not useful work: a
/// queued handshake succeeds where a contended one silently downgrades its unit to ungoverned
/// execution. Set the env override to a large number to disable the gate.
const START_PERMITS_DEFAULT: usize = 2;

/// How long a start waits for a permit before giving up and proceeding contended.
///
/// Deliberately NOT [`session_new_budget`]. That wait is pure overhead spent before the bridge is
/// even spawned, and the waiter still needs its full budget *after* admission — so tying the two
/// together makes them compound, and raising the budget to fix slow handshakes would lengthen the
/// queue in front of them.
///
/// 30s comes from the drain rate the gate itself enforces: held to 2 concurrent starts a handshake
/// costs ~1.75s (K=2 in the table above), so a fan-out of roughly 34 simultaneous units is still
/// admitted inside the bound — well past any run this platform dispatches. Beyond that the gate is
/// no longer the thing helping (a permit holder is stuck near its own budget, or arrivals far
/// exceed what serialising can absorb) and proceeding contended beats waiting longer.
const START_WAIT: Duration = Duration::from_secs(30);

/// Parses a seconds override, falling back to `default`.
///
/// Split from the env lookup so the defaults are testable without the process environment: a test
/// that asserted on [`initialize_budget`] would fail on any host that legitimately sets the
/// override, which is a supported configuration and not a defect.
fn parse_secs(raw: Option<String>, default: u64) -> Duration {
    let secs = raw
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(default);
    Duration::from_secs(secs)
}

fn env_secs(key: &str, default: u64) -> Duration {
    parse_secs(std::env::var(key).ok(), default)
}

fn initialize_budget() -> Duration {
    env_secs("WICKED_ACP_INIT_SECS", INIT_DEFAULT_SECS)
}

fn session_new_budget() -> Duration {
    env_secs("WICKED_ACP_SESSION_NEW_SECS", SESSION_NEW_DEFAULT_SECS)
}

/// Parses the start-concurrency override. Pure, for the same reason as [`parse_secs`].
fn parse_permits(raw: Option<String>) -> usize {
    raw.and_then(|s| s.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(START_PERMITS_DEFAULT)
}

fn start_permits() -> usize {
    parse_permits(std::env::var("WICKED_ACP_START_CONCURRENCY").ok())
}

struct StartGate {
    available: Mutex<usize>,
    released: std::sync::Condvar,
}

impl StartGate {
    fn new(permits: usize) -> Self {
        Self {
            available: Mutex::new(permits),
            released: std::sync::Condvar::new(),
        }
    }

    /// Waits for a permit, but only up to `wait`. On timeout it returns `None` and the caller
    /// proceeds ANYWAY: the gate reduces contention, it is not a correctness barrier, and a
    /// contended handshake that might still succeed beats a queue that looks like a hang. Without
    /// the cap, two bridges stuck for their full budget would stall every other start behind them.
    fn acquire(&'static self, wait: Duration) -> Option<StartPermit> {
        let guard = self.available.lock().unwrap_or_else(|p| p.into_inner());
        let (mut n, _) = self
            .released
            .wait_timeout_while(guard, wait, |n| *n == 0)
            .unwrap_or_else(|p| p.into_inner());
        // Decide on the permit count, NOT on `WaitTimeoutResult::timed_out()`. The two agree here
        // — `wait_timeout_while` only reports a timeout with its predicate still true, and the lock
        // is held from that check to the return — but the count is what actually decides, and
        // reading it directly means this cannot start refusing an available permit if that detail
        // of the std implementation ever shifts.
        if *n == 0 {
            return None;
        }
        *n -= 1;
        Some(StartPermit { gate: self })
    }
}

/// The process-wide gate. Tests build their own [`StartGate`] instead of exhausting this one,
/// so a test that deliberately holds every permit cannot stall a concurrent one.
fn start_gate() -> &'static StartGate {
    static GATE: std::sync::OnceLock<StartGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| StartGate::new(start_permits()))
}

/// A permit to run a handshake. Released on drop, so an early return or a panic mid-handshake
/// cannot leak one.
struct StartPermit {
    gate: &'static StartGate,
}

impl Drop for StartPermit {
    fn drop(&mut self) {
        let mut n = self
            .gate
            .available
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        *n += 1;
        self.gate.released.notify_one();
    }
}

/// The tail of a bridge's stderr, kept so a failed handshake can report what the bridge itself
/// said. Bounded: a chatty bridge must not grow memory in a runner that lives as long as the
/// daemon. Previously stderr was `Stdio::null()`, which is why every failure in this path — a
/// contended handshake, a missing binary, an auth hang — collapsed to one opaque string
/// containing a raw JSON-RPC id.
type StderrTail = Arc<Mutex<std::collections::VecDeque<String>>>;

const STDERR_TAIL_LINES: usize = 20;

/// Per-line byte cap. A line count alone does not bound anything: one bridge writing a single
/// megabyte-long line without a newline would sit in the tail whole. Both bounds together make the
/// rendered tail small enough to append to a capped output without argument.
const STDERR_TAIL_LINE_BYTES: usize = 512;

/// Truncates on a char boundary and SAYS it truncated — a silently clipped line reads as a bridge
/// that stopped mid-sentence, which is a different diagnosis than one that said too much.
fn clip_stderr_line(line: String) -> String {
    if line.len() <= STDERR_TAIL_LINE_BYTES {
        return line;
    }
    let mut cut = STDERR_TAIL_LINE_BYTES;
    while cut > 0 && !line.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…(+{} bytes)", &line[..cut], line.len() - cut)
}

fn drain_stderr(stderr: std::process::ChildStderr) -> (StderrTail, std::thread::JoinHandle<()>) {
    let tail: StderrTail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
    let sink = tail.clone();
    let handle = std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.trim().is_empty() {
                continue;
            }
            let mut t = sink.lock().unwrap_or_else(|p| p.into_inner());
            if t.len() == STDERR_TAIL_LINES {
                t.pop_front();
            }
            t.push_back(clip_stderr_line(line));
        }
    });
    (tail, handle)
}

/// Appends `note` to `output` while keeping `output` within `max_out`, trimming the OLDER text to
/// make room. `handle_update` holds streamed output to that cap and appending must not quietly
/// break it — but the note outranks what it displaces: by the time one is written, `output` is
/// already a truncated fragment of a turn that failed, and the note is the only account of why.
fn append_within_cap(output: &mut String, note: &str, max_out: usize) {
    if output.len() + note.len() > max_out {
        let mut cut = max_out.saturating_sub(note.len()).min(output.len());
        while cut > 0 && !output.is_char_boundary(cut) {
            cut -= 1;
        }
        output.truncate(cut);
    }
    output.push_str(note);
}

/// Renders the tail for an error message, or a note that the bridge said nothing — which is
/// itself diagnostic: silence points at contention or a hang, output points at the bridge.
fn stderr_context(tail: &StderrTail) -> String {
    let t = tail.lock().unwrap_or_else(|p| p.into_inner());
    if t.is_empty() {
        return "; bridge stderr: (silent)".to_string();
    }
    format!(
        "; bridge stderr (last {} of {}): {}",
        t.len(),
        STDERR_TAIL_LINES,
        t.iter().cloned().collect::<Vec<_>>().join(" | ")
    )
}

// ── Session startup ───────────────────────────────────────────────────────────

/// Spawn the ACP binary and complete the `initialize` + `session/new` handshake.
/// Returns `Err` if the binary is not on PATH, the process fails to start, or either
/// handshake call exceeds its budget (see [`initialize_budget`] / [`session_new_budget`]).
///
/// This takes no governance argument. It used to accept one and translate it into `--settings
/// <path>` plus the gate-hook's env vars; the env vars arrived, the flag did not (the bridge does
/// not parse it), so the hook had everything it needed except the instruction to run. Governed units
/// take the wrapped path now — see the fail-closed return in `run_unit_streaming` and FINDING-060.
fn start_acp_process(config: &AcpConfig, cwd: &std::path::Path) -> anyhow::Result<AcpProcess> {
    let build_cmd = |binary: &str| {
        let mut cmd = std::process::Command::new(binary);
        cmd.args(&config.start_args);
        cmd.current_dir(cwd);
        // Same strip the wrapped launcher applies (FINDING-067): an agent CLI that inherits
        // `WICKED_ESTATE_DB` has every estate tool it can spawn pointed at the engine's operational
        // store by default. Governed units do not come through here (they take the wrapped path,
        // FINDING-060), but an ungoverned worker in a repo runs the same `wicked-estate index .`.
        cmd.env_remove(crate::gate_hook::ESTATE_DB_ENV);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd
    };

    // Held for the spawn and both handshake calls, released when this function returns — or NOT
    // held at all, if no permit came free inside `START_WAIT`. That is the designed outcome, not a
    // failure: the start proceeds either way, contended rather than queued.
    let _permit = start_gate().acquire(START_WAIT);

    let mut child = match build_cmd(&config.binary).spawn() {
        Ok(c) => c,
        // Windows: npm installs launcher shims as `<name>.cmd`, which CreateProcess
        // does not resolve for a bare name — retry with the extension explicit
        // (std special-cases explicit .cmd/.bat since the BatBadBut hardening).
        // Only when the configured binary has no extension of its own: appending
        // to `foo.exe` would produce a nonsensical `foo.exe.cmd`.
        Err(e)
            if cfg!(windows)
                && e.kind() == std::io::ErrorKind::NotFound
                && std::path::Path::new(&config.binary).extension().is_none() =>
        {
            let cmd_name = format!("{}.cmd", config.binary);
            build_cmd(&cmd_name).spawn().map_err(|e2| {
                anyhow::anyhow!(
                    "ACP binary '{}': {e} (also tried '{cmd_name}': {e2})",
                    config.binary
                )
            })?
        }
        Err(e) => return Err(anyhow::anyhow!("ACP binary '{}': {e}", config.binary)),
    };

    // Start draining stderr immediately: a bridge that fails during startup writes its reason
    // there, and a piped stream nobody reads eventually blocks the writer.
    let (stderr_tail, stderr_reader) = match child.stderr.take() {
        Some(s) => {
            let (tail, handle) = drain_stderr(s);
            (tail, Some(handle))
        }
        None => (
            Arc::new(Mutex::new(std::collections::VecDeque::new())),
            None,
        ),
    };

    // Take stdout/stdin before spawning the reader — kill the child if either fails so we
    // don't leak a background process when the child started but didn't expose its pipes.
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("ACP binary '{}': no stdout", config.binary));
        }
    };
    let mut stdin = BufWriter::new(match child.stdin.take() {
        Some(s) => s,
        None => {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow::anyhow!("ACP binary '{}': no stdin", config.binary));
        }
    });

    // Unbounded channel — the reader thread never blocks the child on a full buffer.
    let (tx, rx) = std::sync::mpsc::channel();
    let reader_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if !line.is_empty() && tx.send(line).is_err() {
                break;
            }
        }
    });

    // Helper: kills the child and waits before returning a handshake error so we don't leak
    // the background process when initialize/session-new fails or times out. The bridge's own
    // stderr is appended to every one of these — the failure reason reaches `AcpFallback`, which
    // is the operator's only signal that the governed path was abandoned.
    macro_rules! handshake_err {
        ($child:expr, $e:expr) => {{
            let _ = $child.kill();
            let _ = $child.wait();
            let e: anyhow::Error = $e;
            return Err(anyhow::anyhow!("{e}{}", stderr_context(&stderr_tail)));
        }};
    }

    if let Err(e) = rpc_send(
        &mut stdin,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {}, "terminal": false},
            "clientInfo": {"name": "wicked-core", "version": env!("CARGO_PKG_VERSION")}
        }),
    ) {
        handshake_err!(child, e);
    }
    if let Err(e) = rpc_expect(&rx, 1, initialize_budget()) {
        handshake_err!(child, e);
    }

    // `mcpServers` is required by the ACP spec — native ACP agents (copilot --acp)
    // reject session/new with -32602 when it is absent; bridges ignore it.
    if let Err(e) = rpc_send(
        &mut stdin,
        2,
        "session/new",
        json!({
            "cwd": cwd.to_string_lossy().as_ref(),
            "mcpServers": []
        }),
    ) {
        handshake_err!(child, e);
    }
    let resp = match rpc_expect(&rx, 2, session_new_budget()) {
        Ok(v) => v,
        Err(e) => handshake_err!(child, e),
    };
    let session_id = match resp["result"]["sessionId"].as_str() {
        Some(s) => s.to_string(),
        None => handshake_err!(
            child,
            anyhow::anyhow!("ACP session/new: missing sessionId in response")
        ),
    };

    Ok(AcpProcess {
        child,
        stdin,
        line_rx: rx,
        _reader: reader_thread,
        stderr_tail,
        _stderr_reader: stderr_reader,
        session_id,
        next_id: 3,
    })
}

// ── JSON-RPC helpers ──────────────────────────────────────────────────────────

fn rpc_send(
    stdin: &mut BufWriter<ChildStdin>,
    id: u64,
    method: &str,
    params: Value,
) -> anyhow::Result<()> {
    let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
    writeln!(stdin, "{msg}")?;
    stdin.flush()?;
    Ok(())
}

/// Wait for the JSON-RPC response whose `"id"` matches `id`, skipping both
/// notifications and non-JSON startup banners/logs. Returns `Err` on timeout,
/// channel disconnect, or a server-side `"error"` field.
fn rpc_expect(
    rx: &std::sync::mpsc::Receiver<String>,
    id: u64,
    timeout: Duration,
) -> anyhow::Result<Value> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            return Err(anyhow::anyhow!("ACP timeout waiting for response id={id}"));
        }
        match rx.recv_timeout(remaining) {
            Ok(line) => {
                // Skip non-JSON lines (startup banners, log output, etc.) — consistent
                // with exec_turn_acp which also silently skips non-JSON noise.
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if let Some(err) = v.get("error") {
                        return Err(anyhow::anyhow!("ACP server error: {err}"));
                    }
                    return Ok(v);
                }
                // Skip notifications (they have "method" but no matching "id").
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("ACP process exited during handshake"));
            }
        }
    }
}

// ── Turn execution ────────────────────────────────────────────────────────────

struct TurnResult {
    output: String,
    status: StepStatus,
    usage: Option<Usage>,
    files: Vec<String>,
}

/// Send one `session/prompt` request and collect `session/update` notifications until
/// the response arrives (or `timeout` elapses). Streams text deltas through `emit`.
///
/// `prior_outputs` are injected as leading ACP prompt blocks so the agent sees the work this turn is
/// supposed to build on — a peer CLI's output, or (FINDING-024) the output of a phase this one
/// declared `depends_on`. Each block is prefixed with its label so the agent can attribute the
/// contribution, and a contract header precedes them stating that they are the subject of the task.
/// When the slice is empty the prompt stays a single text block exactly as before — no header.
fn exec_turn_acp(
    proc: &mut AcpProcess,
    prompt: &str,
    prior_outputs: &[PriorUnitOutput],
    emit: &DeltaSink,
    timeout: Duration,
) -> anyhow::Result<TurnResult> {
    let id = proc.next_id;
    proc.next_id += 1;

    // Build the prompt block array: a contract header, the prior outputs, then the work prompt.
    let mut blocks: Vec<Value> = Vec::new();
    if !prior_outputs.is_empty() {
        // FINDING-024 (3): STATE the contract; do not let the phase name imply it. Labelled blobs
        // alone were read as background — an `adversarial-review` phase handed the build's output
        // still re-solved the original task, because nothing told it the blob was the subject. The
        // phase name is not an instruction, so this says plainly what the blocks are and what to do
        // with them. Only emitted when there IS prior context, so single-CLI runs with no declared
        // dependency keep the exact prompt they had before.
        blocks.push(json!({
            "type": "text",
            "text": "CONTEXT (prior phases of this run): the block(s) below are the verbatim output \
of earlier phases in this same workflow run. Blocks marked `depends_on` are the artifacts your \
phase explicitly declared it consumes — treat them as the SUBJECT of your task, not as background. \
Build on this work; do not re-solve the original problem from scratch, and do not choose a different \
target than the one the prior phase worked on. If your phase reviews, tests, or revises, it is that \
prior output you are reviewing, testing, or revising."
        }));
    }
    blocks.extend(prior_outputs.iter().map(|p| {
        json!({
            "type": "text",
            "text": format!("{}\n{}", p.label, p.output)
        })
    }));
    blocks.push(json!({"type": "text", "text": prompt}));

    rpc_send(
        &mut proc.stdin,
        id,
        "session/prompt",
        json!({
            "sessionId": proc.session_id,
            "prompt": blocks
        }),
    )?;

    let mut output = String::new();
    let mut usage: Option<Usage> = None;
    let mut files: Vec<String> = Vec::new();
    const MAX_OUT: usize = 8 * 1024 * 1024;

    let deadline = Instant::now() + timeout;
    let (mut found, mut timed_out) = (false, false);

    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            timed_out = true;
            break;
        }
        match proc.line_rx.recv_timeout(remaining) {
            Ok(line) => {
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    if v.get("error").is_some() {
                        // JSON-RPC error response: treat as a failed turn (not cancelled).
                        break;
                    }
                    let stop = v["result"]["stopReason"].as_str().unwrap_or("end_turn");
                    if stop == "cancelled" {
                        timed_out = true;
                    } else {
                        found = true;
                    }
                    // The ecosystem adapters (official claude/codex, pi-acp, native
                    // opencode) report authoritative usage ON THE PROMPT RESULT, not as
                    // inputTokens/outputTokens usage_update notifications — without this
                    // no CliUsage event fires and the studio's Burn panel stays empty.
                    // Prefer it over notification-derived usage; keep any notification
                    // cost (the result shape carries no cost field).
                    if let Some(result_usage) = parse_result_usage(&v["result"]["usage"]) {
                        let cost = usage.as_ref().and_then(|u| u.cost_usd);
                        usage = Some(Usage {
                            cost_usd: cost.or(result_usage.cost_usd),
                            ..result_usage
                        });
                    }
                    break;
                }

                if v.get("method").and_then(Value::as_str) == Some("session/update") {
                    handle_update(&v, emit, &mut output, &mut usage, &mut files, MAX_OUT);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // No `stopReason` and no timeout means the bridge stopped answering — it died mid-turn. Its
    // stderr is the only account of why, and `StepOutput.output` is where an operator looks, so
    // say it there rather than reporting a Failed unit with an empty reason.
    if !found && !timed_out {
        let note = format!(
            "\n[wicked-core] ACP turn ended with no stopReason (the bridge stopped answering){}",
            stderr_context(&proc.stderr_tail)
        );
        append_within_cap(&mut output, &note, MAX_OUT);
    }

    Ok(TurnResult {
        output: output.trim_end().to_string(),
        status: if found {
            StepStatus::Ok
        } else if timed_out {
            StepStatus::Cancelled
        } else {
            StepStatus::Failed
        },
        usage,
        files,
    })
}

/// Process one `session/update` notification — extract text chunks and usage.
fn handle_update(
    v: &Value,
    emit: &DeltaSink,
    output: &mut String,
    usage: &mut Option<Usage>,
    files: &mut Vec<String>,
    max_out: usize,
) {
    let update = &v["params"]["update"];
    let kind = update
        .get("sessionUpdate")
        .and_then(Value::as_str)
        .unwrap_or("");
    match kind {
        "agent_message_chunk" => {
            if let Some(text) = update["content"]["text"].as_str() {
                emit(text);
                let used = output.len();
                if used < max_out {
                    // Clamp to remaining capacity at a valid UTF-8 boundary so
                    // a single large chunk never pushes output past max_out.
                    let remaining = max_out - used;
                    let safe = text
                        .char_indices()
                        .take_while(|(i, c)| *i + c.len_utf8() <= remaining)
                        .last()
                        .map(|(i, c)| i + c.len_utf8())
                        .unwrap_or(0);
                    output.push_str(&text[..safe]);
                }
            }
        }
        "usage_update" => {
            let input = update["inputTokens"]
                .as_u64()
                .or_else(|| update["input_tokens"].as_u64())
                .unwrap_or(0);
            let out = update["outputTokens"]
                .as_u64()
                .or_else(|| update["output_tokens"].as_u64())
                .unwrap_or(0);
            // The official claude adapter's usage_update is `{used, size, cost:{amount}}`
            // — no per-direction tokens (those arrive on the prompt result), but cost is
            // ONLY reported here, so lift it even when the token fields are absent.
            let cost = update["cost"]["amount"].as_f64();
            if input > 0 || out > 0 {
                *usage = Some(Usage {
                    input_tokens: input,
                    output_tokens: out,
                    cost_usd: cost.or_else(|| usage.as_ref().and_then(|u| u.cost_usd)),
                });
            } else if let Some(c) = cost {
                let (i, o) = usage
                    .as_ref()
                    .map(|u| (u.input_tokens, u.output_tokens))
                    .unwrap_or((0, 0));
                *usage = Some(Usage {
                    input_tokens: i,
                    output_tokens: o,
                    cost_usd: Some(c),
                });
            }
        }
        "tool_call_update" => {
            // Collect file paths reported by the CLI (e.g. read/edit locations).
            if let Some(locs) = update["locations"].as_array() {
                for loc in locs {
                    if let Some(path) = loc["path"].as_str() {
                        files.push(path.to_string());
                    }
                }
            }
        }
        _ => {}
    }
}

/// Parse the authoritative usage object from a `session/prompt` RESULT. All ecosystem
/// adapters report here (camelCase): official claude/codex, pi-acp, native opencode.
/// Input counts cached reads/writes alongside fresh input, mirroring the historical
/// bridge semantics (total context presented to the model). `None` when absent/empty.
fn parse_result_usage(u: &Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    let field = |k: &str| u[k].as_u64().unwrap_or(0);
    // Saturating: token counters come from an external process — a malformed or
    // hostile frame must clamp, never wrap into a tiny bogus total.
    let input = field("inputTokens")
        .saturating_add(field("cachedReadTokens"))
        .saturating_add(field("cachedWriteTokens"));
    let output = field("outputTokens");
    if input == 0 && output == 0 {
        return None;
    }
    Some(Usage {
        input_tokens: input,
        output_tokens: output,
        cost_usd: None,
    })
}

// ── Fallback helpers ──────────────────────────────────────────────────────────

/// Run the single-shot fallback, prepending `warning` to the output so it appears in
/// both the streaming view and the persisted `StepOutput.output` (visible in studio).
fn fallback_with_warning(
    warning: String,
    input: &StepInput,
    emit: &DeltaSink,
    fallback: &WrappedCliStepRunner,
) -> StepOutput {
    emit(&format!("{warning}\n"));
    let mut result = fallback.run_unit_streaming(input, emit);
    result.output = if result.output.is_empty() {
        warning
    } else {
        format!("{warning}\n{}", result.output)
    };
    result
}

// ── ACP input governance ──────────────────────────────────────────────────────

// ── ACP input governance (removed) ────────────────────────────────────────────
//
// `arm_acp_governance`, `AcpGovArmed` and `quote_exe_for_hook` lived here. They wrote a per-unit
// settings file (PreToolUse gate-hook + `permissions.deny`) and handed it to the bridge as
// `--settings <path>`. The bridge never read that flag, so the whole mechanism was ceremony: armed,
// announced, never applied (FINDING-060). They are deleted rather than kept behind a feature flag
// because a governance mechanism that compiles but cannot fire is worse than an absent one — it
// reads as coverage. Governed claude units now take the wrapped path, which arms the same hook via
// `execute_wrapped::arm_input_governance` on argv the CLI does read.
//
// Restoring governance to the ACP path means finding a channel the bridge honours — see
// FINDING-062. Whatever that channel turns out to be, the arming code should be written against a
// verified carrier, not resurrected from here.

// ── AcpStepRunner ─────────────────────────────────────────────────────────────

// `None` entries cache a failed startup so subsequent units for the same
// `(run_id, cli_key)` fall back immediately without re-attempting spawn.
type SessionMap = Arc<Mutex<HashMap<(String, String), Option<Arc<Mutex<AcpProcess>>>>>>;

/// A [`StepRunner`] that drives ACP multi-turn sessions for all registered CLIs.
///
/// Sessions are keyed by `(run_id, cli_key)` — each CLI in a multi-CLI run gets its own
/// persistent ACP process so units are never mis-routed to the wrong agent.
///
/// Falls back to [`WrappedCliStepRunner`] (single-shot) when:
/// - the unit is governed and runs claude — governance only holds on the wrapped path (FINDING-060)
/// - the CLI has no ACP config in the registry
/// - the ACP binary is not on PATH
/// - the handshake fails or the session dies mid-run
///
/// All fallbacks prepend a `[wicked-core] ACP …` warning to `StepOutput.output` so
/// the degradation is visible in both streaming output and persisted logs.
/// Stable `fallback_kind` slugs carried on [`CoreEvent::AcpFallback`] for UI dispatch.
pub(crate) mod fallback_kind {
    pub const BINARY_UNAVAILABLE: &str = "binary_unavailable";
    pub const SESSION_DIED: &str = "session_died";
    pub const HTTP_UNIMPLEMENTED: &str = "http_unimplemented";
    /// A governed claude unit, routed to the wrapped path on purpose. Not a failure — nothing broke
    /// — but it IS a behaviour change the operator has to be able to see: the unit runs single-shot
    /// instead of multi-turn, and the reason is that the ACP bridge cannot carry input governance.
    /// Emitting nothing here would make "governed units are slower" an unexplained mystery, which is
    /// how the ungoverned ACP path went unnoticed in the first place.
    pub const GOVERNANCE_REQUIRES_WRAPPED: &str = "governance_requires_wrapped";
    // RETIRED: `handshake_failed`. Its only emitter was the governed-ACP branch removed with
    // FINDING-060. The shared session path reports every startup failure — spawn or handshake — as
    // `binary_unavailable`, so the slug is no longer produced; a consumer still switching on it is
    // waiting for an event that cannot arrive.
}

/// Operator messages queued per run for next-turn delivery: `(original target, message)`.
type InjectQueue = Arc<Mutex<HashMap<String, Vec<(crate::command::InjectTarget, String)>>>>;

pub struct AcpStepRunner {
    /// Back-channel to the actor's single emit point (relay via `Command::EmitEvent`).
    tx: std::sync::mpsc::Sender<Command>,
    /// Keyed by `(run_id, cli_key)` — one process per CLI per run.
    sessions: SessionMap,
    /// Operator messages queued for delivery on the run's next matching unit prompt
    /// (the ACP inject path — there is no PTY to write into mid-turn). Keyed by run_id;
    /// drained in [`AcpStepRunner::exec_turn`], pruned with the run's sessions.
    pending_injects: InjectQueue,
    /// Last activity per CHAT id — set on open, on every ensure, and on every turn.
    ///
    /// Idleness is a property of the chat, not of a seat: `chat_close` reaps a whole chat, so that
    /// is the granularity a reaper can act on. Kept beside the pool rather than inside it because
    /// the pool is keyed per seat and a chat with zero warm seats still needs a last-touch (it may
    /// be mid-`chat_open`, warming its first seat).
    chat_activity: Arc<Mutex<HashMap<String, Instant>>>,
    fallback: WrappedCliStepRunner,
    timeout: Duration,
}

/// Why a chat's warm sessions were released — carried on `ChatClosed` so an operator can tell a
/// chat they ended from one the daemon reclaimed underneath them (FINDING-027).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChatCloseReason {
    /// An explicit close from the operator or the UI.
    Requested,
    /// Reclaimed after `WICKED_CHAT_IDLE_SECS` with no turn.
    Idle,
    /// Evicted as the least-recently-used chat when the pool reached `WICKED_CHAT_POOL_MAX`.
    PoolCap,
}

impl ChatCloseReason {
    /// The wire token. Stable — consumers branch on it.
    pub fn as_str(self) -> &'static str {
        match self {
            ChatCloseReason::Requested => "requested",
            ChatCloseReason::Idle => "idle",
            ChatCloseReason::PoolCap => "pool_cap",
        }
    }
}

/// One live chat, for the enumerate surface. A leak nobody can list is a leak nobody can reclaim
/// (FINDING-027 gap 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatInfo {
    pub chat_id: String,
    /// The seats currently warm, sorted.
    pub seats: Vec<String>,
    /// Seconds since the last open/ensure/turn on this chat.
    pub idle_secs: u64,
}

impl AcpStepRunner {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<Command>) -> Self {
        let secs = std::env::var("WICKED_UNIT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(7200);
        Self {
            // Give the fallback runner the same tx so it can relay GovernanceContextArmed
            // events (EVT-016 "wrapped_cli" path) when ACP falls back to the wrapped-CLI runner.
            fallback: WrappedCliStepRunner::with_tx(tx.clone()),
            tx,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            pending_injects: Arc::new(Mutex::new(HashMap::new())),
            chat_activity: Arc::new(Mutex::new(HashMap::new())),
            timeout: Duration::from_secs(secs),
        }
    }

    fn emit_event(&self, ev: CoreEvent) {
        let _ = self.tx.send(Command::EmitEvent(ev));
    }

    // ── Chat sessions (crew#165 / core#13) ──────────────────────────────────────
    //
    // A chat reuses the SAME session pool as runs, keyed `("chat:<id>", cli)`. Turns
    // are RAW conversation — no governance arming, no council, no unit machinery, no
    // wrapped-CLI fallback (a dead seat is reported honestly and re-warmed on the
    // next ensure, never silently downgraded to one-shot).

    fn chat_pool_key(chat_id: &str) -> String {
        format!("chat:{chat_id}")
    }

    fn chat_timeout() -> Duration {
        let secs = std::env::var("WICKED_CHAT_TURN_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(300);
        Duration::from_secs(secs)
    }

    /// How long a chat may sit with no turn before the reaper reclaims it.
    ///
    /// This is NOT [`Self::chat_timeout`]: that is a per-turn response budget and never fires on a
    /// chat nobody is talking to. Idle eviction is the only reclamation path that covers a chat
    /// orphaned by a closed or crashed tab, which no client-side teardown can reach (FINDING-027).
    ///
    /// 30 minutes by default: a warm seat costs ~520 MB resident, and a chat untouched for half an
    /// hour is far more likely abandoned than mid-thought. Re-warming is a few seconds.
    pub fn chat_idle_ttl() -> Duration {
        let secs = std::env::var("WICKED_CHAT_IDLE_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(1800);
        Duration::from_secs(secs)
    }

    /// The most chats that may hold warm seats at once. A backstop for the case the TTL cannot
    /// cover: many chats opened faster than the idle window retires them.
    ///
    /// Floored at 1 — a cap of 0 would evict the chat being opened, which is not a smaller pool but
    /// a broken one.
    pub fn chat_pool_cap() -> usize {
        std::env::var("WICKED_CHAT_POOL_MAX")
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(8)
            .max(1)
    }

    /// Mark a chat as active NOW. Called on open, ensure, and turn — anything that proves someone
    /// is still using it.
    fn chat_touch(&self, chat_id: &str) {
        let mut guard = self.chat_activity.lock().unwrap_or_else(|p| p.into_inner());
        guard.insert(chat_id.to_string(), Instant::now());
    }

    /// Every chat holding pool entries, with how long it has been idle.
    ///
    /// Chats are enumerated from the SESSION POOL, not from the activity map: the pool is what
    /// actually pins processes, so this can never report a chat that costs nothing while missing
    /// one that does.
    ///
    /// A chat with a pool entry but no WARM seat still lists, with `seats` empty. That differs from
    /// [`Self::chat_seats`] on purpose: `seats` answers "who can take a turn", this answers "what
    /// is holding pool state". Anything in the map is something only a close removes, so anything
    /// in the map has to be listable and reapable — an entry no surface reports is the shape of the
    /// leak this whole mechanism exists to end.
    pub fn chat_list(&self) -> Vec<ChatInfo> {
        let mut by_chat: HashMap<String, Vec<String>> = HashMap::new();
        {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            for ((rid, cli), slot) in guard.iter() {
                let Some(chat_id) = rid.strip_prefix("chat:") else {
                    continue;
                };
                let seats = by_chat.entry(chat_id.to_string()).or_default();
                if slot.is_some() {
                    seats.push(cli.clone());
                }
            }
        }
        let now = Instant::now();
        let activity = self
            .chat_activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let mut out: Vec<ChatInfo> = by_chat
            .into_iter()
            .map(|(chat_id, mut seats)| {
                seats.sort();
                // A warm chat with no recorded activity is treated as idle-since-forever rather
                // than as fresh: the conservative reading reclaims it, and the alternative would
                // let any gap in touch-recording pin memory permanently — the exact defect here.
                let idle_secs = activity
                    .get(&chat_id)
                    .map(|t| now.saturating_duration_since(*t).as_secs())
                    .unwrap_or(u64::MAX);
                ChatInfo {
                    chat_id,
                    seats,
                    idle_secs,
                }
            })
            .collect();
        out.sort_by(|a, b| a.chat_id.cmp(&b.chat_id));
        out
    }

    /// Close every chat idle longer than `ttl`. Returns the ids reaped, oldest first.
    pub fn chat_reap_idle(&self, ttl: Duration) -> Vec<String> {
        let ttl_secs = ttl.as_secs();
        let mut victims: Vec<(u64, String)> = self
            .chat_list()
            .into_iter()
            .filter(|c| c.idle_secs >= ttl_secs)
            .map(|c| (c.idle_secs, c.chat_id))
            .collect();
        // Oldest first, so a caller reading the returned list sees them in the order they aged out.
        victims.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        let reaped: Vec<String> = victims
            .into_iter()
            .map(|(_, id)| {
                self.chat_close(&id, ChatCloseReason::Idle);
                id
            })
            .collect();
        self.prune_orphan_activity(ttl);
        reaped
    }

    /// Drop activity entries with no pool entry behind them.
    ///
    /// `chat_close` prunes the chat it closes, but a turn outliving the TTL re-touches on the way
    /// out — after the reaper already closed the chat — leaving an entry nothing else collects.
    /// Tiny individually; unbounded over a long-lived daemon, which is the shape of bug this whole
    /// change exists to end.
    ///
    /// Only entries older than `ttl` are dropped. `chat_ensure` touches BEFORE inserting its pool
    /// entry, so a chat mid-open is briefly touched-but-unpooled; pruning on absence alone would
    /// race with it, erase its timestamp, and get it reaped on the next sweep as
    /// idle-since-forever. A just-touched entry is never old enough to qualify.
    fn prune_orphan_activity(&self, ttl: Duration) {
        let pooled: std::collections::HashSet<String> =
            self.chat_list().into_iter().map(|c| c.chat_id).collect();
        let now = Instant::now();
        let mut activity = self.chat_activity.lock().unwrap_or_else(|p| p.into_inner());
        activity.retain(|id, t| pooled.contains(id) || now.saturating_duration_since(*t) < ttl);
    }

    /// Evict least-recently-used chats until at most `cap` remain. Returns the ids evicted.
    pub fn chat_enforce_cap(&self, cap: usize) -> Vec<String> {
        let cap = cap.max(1);
        let mut live = self.chat_list();
        let Some(excess) = live.len().checked_sub(cap).filter(|n| *n > 0) else {
            return Vec::new();
        };
        // Most idle first. The caller touches the chat it is opening BEFORE calling this, so that
        // chat sorts last and opening a chat can never evict the chat being opened.
        live.sort_by(|a, b| {
            b.idle_secs
                .cmp(&a.idle_secs)
                .then_with(|| a.chat_id.cmp(&b.chat_id))
        });
        live.into_iter()
            .take(excess)
            .map(|c| {
                self.chat_close(&c.chat_id, ChatCloseReason::PoolCap);
                c.chat_id
            })
            .collect()
    }

    /// Warm (or return the existing) ACP session for one chat seat. Unlike the run
    /// path, a failed start is NOT cached as poisoned — chats are interactive, so
    /// every ensure retries and the operator sees each failure.
    fn chat_ensure(
        &self,
        chat_id: &str,
        cli_key: &str,
        cwd: &std::path::Path,
    ) -> Result<Arc<Mutex<AcpProcess>>, String> {
        // Touch FIRST, and unconditionally: a chat whose seat is warming is in use, and recording
        // that only on success would leave a chat mid-`chat_open` looking idle-since-forever to a
        // reaper running concurrently.
        self.chat_touch(chat_id);
        let key = (Self::chat_pool_key(chat_id), cli_key.to_string());
        {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(Some(arc)) = guard.get(&key) {
                return Ok(arc.clone());
            }
        }
        let config =
            acp_config_for(cli_key).ok_or_else(|| format!("no ACP config for '{cli_key}'"))?;
        if config.transport == AcpTransport::Http {
            return Err(format!(
                "ACP HTTP transport not supported for chat ('{cli_key}')"
            ));
        }
        let proc = start_acp_process(&config, cwd).map_err(|e| e.to_string())?;
        let arc = Arc::new(Mutex::new(proc));
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        // A racing ensure may have inserted first — reuse theirs, drop ours.
        if let Some(Some(existing)) = guard.get(&key) {
            return Ok(existing.clone());
        }
        guard.insert(key, Some(arc.clone()));
        Ok(arc)
    }

    /// Eagerly warm one session per seat; per-seat outcome, `ChatSessionReady`/
    /// `ChatSessionFailed` emitted for each.
    pub fn chat_open(
        &self,
        chat_id: &str,
        clis: &[String],
        cwd: &std::path::Path,
    ) -> Vec<(String, Result<(), String>)> {
        let opened: Vec<(String, Result<(), String>)> = clis
            .iter()
            .map(|cli| {
                let outcome = self.chat_ensure(chat_id, cli, cwd).map(|_| ());
                match &outcome {
                    Ok(()) => self.emit_event(CoreEvent::ChatSessionReady {
                        chat: chat_id.to_string(),
                        cli_key: cli.clone(),
                    }),
                    Err(reason) => self.emit_event(CoreEvent::ChatSessionFailed {
                        chat: chat_id.to_string(),
                        cli_key: cli.clone(),
                        reason: reason.clone(),
                    }),
                }
                (cli.clone(), outcome)
            })
            .collect();
        // Enforce the cap only AFTER the new chat is warm and touched, so it is the freshest entry
        // and therefore the last possible victim. Doing it first would let a full pool evict a
        // chat, warm the new one, and leave the pool at the cap anyway — same memory, one more
        // reap. Cap breaches are rare, so paying the reap on the open path costs nothing typical.
        //
        // Evictions are not logged here: each one emits `ChatClosed { reason: "pool_cap" }`, which
        // is the surface an operator actually watches. A second, log-only channel would be the one
        // that goes stale.
        self.chat_enforce_cap(Self::chat_pool_cap());
        opened
    }

    /// One seat's turn on a chat message. Streams deltas via `ChatDelta`, returns the
    /// completed reply text. On failure the seat's session is EVICTED (next ensure
    /// re-warms) and the error is returned — never floored, never faked.
    pub fn chat_turn(
        &self,
        chat_id: &str,
        cli_key: &str,
        text: &str,
        cwd: &std::path::Path,
    ) -> Result<String, String> {
        let arc = self.chat_ensure(chat_id, cli_key, cwd)?;
        let tx = self.tx.clone();
        let (chat_ev, cli_ev) = (chat_id.to_string(), cli_key.to_string());
        let emit: Box<crate::workflow::DeltaSink> = Box::new(move |delta: &str| {
            let _ = tx.send(Command::EmitEvent(CoreEvent::ChatDelta {
                chat: chat_ev.clone(),
                cli_key: cli_ev.clone(),
                text: delta.to_string(),
            }));
        });
        let result = {
            let mut proc = arc.lock().unwrap_or_else(|p| p.into_inner());
            exec_turn_acp(&mut proc, text, &[], &emit, Self::chat_timeout())
        };
        // Touch again on the way out. `chat_ensure` touched on the way in, but a long turn would
        // then be counted as idle for its whole duration — a 40-minute agent turn would be reaped
        // out from under the operator the moment it finished.
        self.chat_touch(chat_id);
        match result {
            Ok(turn) if turn.status == StepStatus::Ok => Ok(turn.output),
            Ok(turn) => {
                self.chat_evict(chat_id, cli_key);
                Err(format!(
                    "seat '{cli_key}' turn ended {:?}: {}",
                    turn.status, turn.output
                ))
            }
            Err(e) => {
                self.chat_evict(chat_id, cli_key);
                Err(format!("seat '{cli_key}' session error: {e}"))
            }
        }
    }

    fn chat_evict(&self, chat_id: &str, cli_key: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(&(Self::chat_pool_key(chat_id), cli_key.to_string()));
    }

    /// The seats currently warm for a chat (fan-out default for `targets: None`).
    pub fn chat_seats(&self, chat_id: &str) -> Vec<String> {
        let prefix = Self::chat_pool_key(chat_id);
        let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let mut seats: Vec<String> = guard
            .iter()
            .filter(|((rid, _), slot)| rid == &prefix && slot.is_some())
            .map(|((_, cli), _)| cli.clone())
            .collect();
        seats.sort();
        seats
    }

    /// Close a chat's warm sessions and reap their processes. Idempotent.
    ///
    /// `reason` reaches the operator on `ChatClosed`: a chat that vanished because the daemon
    /// reclaimed it is a different event from one the operator ended, and a UI that cannot tell
    /// them apart reports a reclaim as a mystery.
    pub fn chat_close(&self, chat_id: &str, reason: ChatCloseReason) {
        let prefix = Self::chat_pool_key(chat_id);
        {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.retain(|(rid, _), _| rid != &prefix);
        }
        // Drop the activity entry too. It is small, but it is keyed by an unbounded stream of
        // client-minted chat ids — leaving it behind trades a 520 MB leak for a slower one.
        self.chat_activity
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(chat_id);
        self.emit_event(CoreEvent::ChatClosed {
            chat: chat_id.to_string(),
            reason: reason.as_str().to_string(),
        });
    }

    /// Close all ACP sessions for `run_id` and kill their child processes. Idempotent.
    /// Call this after the last unit of a run completes (mirrors
    /// [`PersistentStepRunner::drop_session`]).
    pub fn drop_session(&self, run_id: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard.retain(|(rid, _), _| rid != run_id);
        let mut injects = self
            .pending_injects
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        injects.remove(run_id);
    }

    /// Drain queued operator messages matching `(run_id, cli_key)` — `All`-targeted and
    /// exact-CLI-targeted entries deliver; entries for other CLIs stay queued. Each entry
    /// is returned as `(original target string, prompt-ready block)` so the delivery event
    /// carries the INJECTION target ("all" or the cli_key the operator named), matching
    /// the PTY path's event contract.
    fn drain_operator_messages(
        &self,
        run_id: &str,
        cli_key: &str,
    ) -> Vec<(String, PriorUnitOutput)> {
        use crate::command::InjectTarget;
        let mut guard = self
            .pending_injects
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let Some(queue) = guard.get_mut(run_id) else {
            return Vec::new();
        };
        let mut delivered = Vec::new();
        queue.retain(|(target, message)| {
            let (matches, target_str) = match target {
                InjectTarget::All => (true, "all".to_string()),
                InjectTarget::Cli(k) => (k == cli_key, k.clone()),
            };
            if matches {
                delivered.push((
                    target_str,
                    PriorUnitOutput {
                        label: "[operator message]".to_string(),
                        output: message.clone(),
                    },
                ));
            }
            !matches
        });
        if queue.is_empty() {
            guard.remove(run_id);
        }
        delivered
    }

    fn exec_turn(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        let run_id = input.run_id.clone();
        let cli_key = input
            .unit
            .assigned_cli
            .as_deref()
            .unwrap_or("claude")
            .to_string();

        // Deliver queued operator messages on this turn (the inject path for ACP runs):
        // appended AFTER the cross-CLI context blocks so they read as the most recent
        // guidance. Consumed here even if the turn later falls back to the wrapped path —
        // the same at-most-once posture as the cross-CLI context those paths also drop.
        let operator_msgs = self.drain_operator_messages(&run_id, &cli_key);
        for (orig_target, block) in &operator_msgs {
            self.emit_event(CoreEvent::WorkerMessageInjected {
                session: run_id.clone(),
                message: block.output.clone(),
                target: orig_target.clone(),
            });
        }
        let prior_with_operator: Vec<PriorUnitOutput>;
        let prior_outputs: &[PriorUnitOutput] = if operator_msgs.is_empty() {
            &input.prior_outputs
        } else {
            prior_with_operator = input
                .prior_outputs
                .iter()
                .cloned()
                .chain(operator_msgs.into_iter().map(|(_, block)| block))
                .collect();
            &prior_with_operator
        };

        // GOVERNED UNITS DO NOT RUN ON ACP — they take the wrapped-CLI path, which is the only
        // path where input governance is measured to hold (FINDING-060 / FINDING-061).
        //
        // What this replaced: an "armed" ACP path that wrote a per-unit settings file carrying the
        // PreToolUse gate-hook and the `permissions.deny` fence, passed it to the bridge as
        // `--settings <path>`, and emitted `GovernanceContextArmed { path: "acp" }`. The bridge does
        // not read that flag. `@agentclientprotocol/claude-agent-acp@0.62` inspects argv for exactly
        // four things — `--cli`, `--version`, `-v`, `--hide-claude-auth` — and `--settings` is
        // accepted as unknown argv and discarded; the `claude` it then spawns carries no `--settings`
        // of its own. Measured on one live run whose units split across both paths, sharing a single
        // decisions log: 33 gate-hook firings across the two wrapped-fallback units, 0 across the two
        // ACP units, while all four were announced as armed. The ACP units were not idle — one burned
        // 4.1M input tokens editing files. Every one of those tool calls was ungoverned, and the
        // engine recorded `governed: true` for them.
        //
        // The bridge also hardcodes `settingSources: ["user", "project", "local"]` and resolves the
        // permission mode from `permissions.defaultMode` in those settings, so an ACP worker inherits
        // the operator's user scope — the leak FINDING-047 exists to close, still open here because
        // `inject_isolation_flags` only runs on the wrapped path. An operator whose settings say
        // `dontAsk` gets workers with Read/Edit/Write denied; observed consequence was every file
        // mutation rerouted through Bash, which no file-tool deny rule can see, and one unit that
        // silently applied nothing and still reported done.
        //
        // Falling back is the same decision the HTTP-transport branch below already makes for the
        // same reason ("--settings cannot be injected"). The stdio branch assumed injection worked
        // because the flag was accepted. Accepted is not applied.
        //
        // The cost is real: governed units lose multi-turn ACP and run single-shot. That is the
        // deliberate trade — the engine's contract is that a governed unit MUST NOT run ungoverned,
        // and multi-turn is a performance property. Restoring it needs a channel the bridge actually
        // honours (`_meta.claudeCode.options`, or answering `session/request_permission` from the
        // gate); filed as FINDING-062, and it is a capability addition, not a precondition for this.
        //
        // Non-claude CLIs are unaffected: input arming was always claude-only, so their governed
        // units keep the shared ACP session path below exactly as before.
        if input.governance.is_some() && cli_runs_claude(&cli_key) {
            // Warned, not just evented. A governed unit that quietly loses multi-turn looks
            // identical in `StepOutput.output` to one that never had it, and an unobserved path
            // change is precisely how the ungoverned ACP path survived this long.
            let reason = format!(
                "[wicked-core] governed unit for '{cli_key}' runs single-shot: the ACP bridge \
                 does not apply the input-governance settings it is given"
            );
            self.emit_event(CoreEvent::AcpFallback {
                session: run_id.clone(),
                cli_key: cli_key.clone(),
                reason: reason.clone(),
                fallback_kind: fallback_kind::GOVERNANCE_REQUIRES_WRAPPED.to_string(),
            });
            return fallback_with_warning(reason, input, emit, &self.fallback);
        }

        // SHARED ACP SESSION PATH — ungoverned units of every CLI, plus governed units of
        // non-claude CLIs (input arming is claude-only; their output-side gates still run).
        let acp_config = match acp_config_for(&cli_key) {
            Some(c) => c,
            None => return self.fallback.run_unit_streaming(input, emit),
        };

        if acp_config.transport == AcpTransport::Http {
            let reason = format!(
                "[wicked-core] ACP HTTP transport not yet implemented for '{cli_key}'; \
                 using single-shot fallback"
            );
            self.emit_event(CoreEvent::AcpFallback {
                session: run_id.clone(),
                cli_key: cli_key.clone(),
                reason: reason.clone(),
                fallback_kind: fallback_kind::HTTP_UNIMPLEMENTED.to_string(),
            });
            return fallback_with_warning(reason, input, emit, &self.fallback);
        }

        // Lazily open a session for (run_id, cli_key). The global map lock is held only
        // for the brief map lookup/insert — not across the blocking spawn + handshake.
        let session_key = (run_id.clone(), cli_key.clone());
        // `None` in the map means a previous startup for this key failed; fall back
        // immediately without re-attempting spawn (avoids repeated warnings per run).
        let proc_arc: Arc<Mutex<AcpProcess>> = {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(slot) = guard.get(&session_key) {
                match slot {
                    Some(arc) => arc.clone(),
                    None => {
                        drop(guard); // release sessions lock before the blocking fallback call
                        return self.fallback.run_unit_streaming(input, emit);
                    }
                }
            } else {
                drop(guard);
                let cwd = input
                    .workdir
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                match start_acp_process(&acp_config, &cwd) {
                    Ok(proc) => {
                        let acp_session_id = proc.session_id.clone();
                        let arc = Arc::new(Mutex::new(proc));
                        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                        use std::collections::hash_map::Entry;
                        let (result, did_insert) = match guard.entry(session_key.clone()) {
                            Entry::Vacant(v) => {
                                let slot = v.insert(Some(arc.clone()));
                                (slot.as_ref().unwrap().clone(), true)
                            }
                            Entry::Occupied(o) => (o.into_mut().as_ref().unwrap().clone(), false),
                        };
                        drop(guard);
                        if did_insert {
                            self.emit_event(CoreEvent::AcpSessionStarted {
                                session: run_id.clone(),
                                cli_key: cli_key.clone(),
                                acp_session_id,
                            });
                        }
                        result
                    }
                    Err(e) => {
                        let reason = format!(
                            "[wicked-core] ACP unavailable for '{cli_key}' ({e}); \
                             using single-shot fallback"
                        );
                        {
                            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                            guard.entry(session_key.clone()).or_insert(None);
                        } // release sessions lock before the blocking fallback call
                        self.emit_event(CoreEvent::AcpFallback {
                            session: run_id.clone(),
                            cli_key: cli_key.clone(),
                            reason: reason.clone(),
                            fallback_kind: fallback_kind::BINARY_UNAVAILABLE.to_string(),
                        });
                        return fallback_with_warning(reason, input, emit, &self.fallback);
                    }
                }
            }
        };

        let mut proc = proc_arc.lock().unwrap_or_else(|p| p.into_inner());
        let prompt = unit_prompt(input);

        match exec_turn_acp(&mut proc, &prompt, prior_outputs, emit, self.timeout) {
            Ok(result) if result.status == StepStatus::Ok => StepOutput {
                run_id: input.run_id.clone(),
                unit_ix: input.unit_ix,
                attempt: input.attempt,
                output: result.output,
                status: StepStatus::Ok,
                usage: result.usage,
                files: result.files,
                governed: false,
            },
            Ok(result) if result.status == StepStatus::Cancelled => {
                // Timeout — drop the session: the reader thread may wedge on a full pipe
                // if we leave the ACP process running while no longer consuming its output.
                drop(proc);
                self.drop_session(&run_id);
                StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: result.output,
                    status: StepStatus::Cancelled,
                    usage: result.usage,
                    files: result.files,
                    governed: false,
                }
            }
            Ok(_) => {
                drop(proc);
                self.drop_session(&run_id);
                let reason = format!(
                    "[wicked-core] ACP session exited for '{cli_key}'; \
                     using single-shot fallback"
                );
                self.emit_event(CoreEvent::AcpFallback {
                    session: run_id.clone(),
                    cli_key: cli_key.clone(),
                    reason: reason.clone(),
                    fallback_kind: fallback_kind::SESSION_DIED.to_string(),
                });
                fallback_with_warning(reason, input, emit, &self.fallback)
            }
            Err(e) => {
                drop(proc);
                self.drop_session(&run_id);
                let reason = format!(
                    "[wicked-core] ACP error for '{cli_key}' ({e}); \
                     using single-shot fallback"
                );
                self.emit_event(CoreEvent::AcpFallback {
                    session: run_id.clone(),
                    cli_key: cli_key.clone(),
                    reason: reason.clone(),
                    fallback_kind: fallback_kind::SESSION_DIED.to_string(),
                });
                fallback_with_warning(reason, input, emit, &self.fallback)
            }
        }
    }
}

impl Default for AcpStepRunner {
    fn default() -> Self {
        let (tx, _rx) = std::sync::mpsc::channel();
        Self::new(tx)
    }
}

impl StepRunner for AcpStepRunner {
    fn queue_operator_message(
        &self,
        run_id: &str,
        target: &crate::command::InjectTarget,
        message: &str,
    ) -> bool {
        let mut guard = self
            .pending_injects
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        guard
            .entry(run_id.to_string())
            .or_default()
            .push((target.clone(), message.to_string()));
        true
    }

    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let noop = |_: &str| {};
        self.exec_turn(input, &noop)
    }

    fn run_unit_streaming(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        self.exec_turn(input, emit)
    }

    /// Close all ACP sessions for `run_id` so Claude processes don't leak after a run ends.
    ///
    /// Runs cleanup on a background thread — `on_run_complete` is called from the actor thread
    /// (via `finalize_run`/`fail_run`/`cancel_run`). Dropping `AcpProcess` calls `kill()` +
    /// `wait()` on the child process, which blocks. Doing that on the actor thread would stall
    /// the entire actor while waiting for the subprocess to exit.
    fn on_run_complete(&self, run_id: &str) {
        let sessions = self.sessions.clone();
        let pending_injects = self.pending_injects.clone();
        let run_id = run_id.to_string();
        std::thread::spawn(move || {
            let mut guard = sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.retain(|(rid, _), _| *rid != run_id);
            drop(guard);
            let mut injects = pending_injects.lock().unwrap_or_else(|p| p.into_inner());
            injects.remove(&run_id);
        });
    }

    /// Close a single ACP session for `(run_id, cli_key)` — called by `ReassignUnit` before
    /// re-dispatching to a different CLI. Runs on a background thread (drop may block on kill/wait).
    fn close_cli_session(&self, run_id: &str, cli_key: &str) {
        let sessions = self.sessions.clone();
        let run_id = run_id.to_string();
        let cli_key = cli_key.to_string();
        std::thread::spawn(move || {
            // Remove under the lock, then drop the process value OUTSIDE the lock so the
            // ACP child's Drop impl (kill + wait) never holds the mutex while blocking.
            let removed = {
                let mut guard = sessions.lock().unwrap_or_else(|p| p.into_inner());
                guard.remove(&(run_id, cli_key))
            };
            drop(removed);
        });
    }
}

// ── Registry helper ───────────────────────────────────────────────────────────

/// The merged-registry record for `cli_key` (built-ins + user overlay). Deliberately
/// NOT `registry_roster()`: that filters to `enabled_for_council` seats (a seat disabled
/// for voting can still execute units over ACP) and swallows load errors. A malformed
/// overlay falls back to built-ins instead of stripping every ACP config.
fn registry_record(cli_key: &str) -> Option<wicked_council::AgenticCli> {
    let user = wicked_council::registry::default_user_path();
    wicked_council::registry::load(user.as_deref())
        .unwrap_or_else(|_| wicked_council::registry::builtin())
        .into_iter()
        .find(|c| c.key == cli_key)
}

fn acp_config_for(cli_key: &str) -> Option<AcpConfig> {
    // The MERGED registry, not builtin(): a user record replaces its built-in wholesale,
    // so its [cli.acp] table (or its absence) must decide the transport here exactly as
    // it does everywhere else.
    registry_record(cli_key).and_then(|c| c.acp)
}

/// Whether this seat runs Claude Code — classified by the REGISTERED BINARY, not the
/// key: `binary_is_claude` is a path-stem classifier and registry keys can diverge from
/// binary names (e.g. a "claude-eval" seat whose binary is `claude`). Ad-hoc CLIs not
/// in the registry fall back to classifying the key itself (the historical behaviour).
fn cli_runs_claude(cli_key: &str) -> bool {
    match registry_record(cli_key) {
        Some(c) => crate::execute_wrapped::binary_is_claude(&c.binary),
        None => crate::execute_wrapped::binary_is_claude(cli_key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::command::InjectTarget;

    fn runner() -> AcpStepRunner {
        let (tx, _rx) = std::sync::mpsc::channel();
        AcpStepRunner::new(tx)
    }

    // ── FINDING-022: handshake budgets, start gate, stderr capture ──────────────

    #[test]
    fn the_two_handshake_calls_have_separate_budgets_and_neither_is_the_old_10s_constant() {
        // The defect was ONE constant covering two calls with different cost profiles, at a value
        // that sat inside the measured spread of the slower one. Pinning both to >10s is what
        // stops a reviewer reinstating a single shared constant at the old value.
        //
        // Asserted on the defaults, not on `initialize_budget()` / `session_new_budget()`: those
        // read the process env, and an override is a supported configuration — a host that sets
        // one must not fail a test about what the code ships with.
        assert!(parse_secs(None, INIT_DEFAULT_SECS) > Duration::from_secs(10));
        assert!(parse_secs(None, SESSION_NEW_DEFAULT_SECS) > Duration::from_secs(10));
    }

    #[test]
    fn a_budget_override_must_be_a_positive_number_or_the_default_stands() {
        // `parse_secs` runs per handshake, so a typo'd or zero override must not silently become
        // an instant timeout — that would fail EVERY handshake open and downgrade every unit to
        // ungoverned execution, which is the exact failure this whole change exists to stop.
        let default = Duration::from_secs(60);
        assert_eq!(parse_secs(None, 60), default, "unset");
        assert_eq!(parse_secs(Some("0".into()), 60), default, "zero");
        assert_eq!(parse_secs(Some("".into()), 60), default, "empty");
        assert_eq!(parse_secs(Some("ninety".into()), 60), default, "garbage");
        assert_eq!(parse_secs(Some("-5".into()), 60), default, "negative");
        assert_eq!(
            parse_secs(Some("90".into()), 60),
            Duration::from_secs(90),
            "valid"
        );
    }

    #[test]
    fn the_permit_wait_is_not_tied_to_the_budget_of_the_call_it_guards() {
        // Coupling them compounds: the wait is spent BEFORE the bridge is spawned and the waiter
        // still needs its full budget after admission, so raising the budget to fix slow
        // handshakes would also lengthen the queue in front of them.
        assert!(
            START_WAIT < parse_secs(None, SESSION_NEW_DEFAULT_SECS),
            "the permit wait must stay strictly under the budget of the call it guards"
        );
    }

    /// A gate of its own per test — exhausting the process-wide one would stall any concurrent
    /// test that starts a bridge, which is the very failure mode these tests exist to prevent.
    fn test_gate(permits: usize) -> &'static StartGate {
        Box::leak(Box::new(StartGate::new(permits)))
    }

    #[test]
    fn the_start_gate_hands_out_a_bounded_number_of_permits_and_reclaims_them_on_drop() {
        let gate = test_gate(2);
        let held: Vec<StartPermit> = (0..2)
            .map(|_| gate.acquire(Duration::from_secs(5)).expect("a free permit"))
            .collect();
        // Exhausted: the next caller waits rather than piling onto the contended handshake.
        assert!(
            gate.acquire(Duration::from_millis(50)).is_none(),
            "the gate handed out more than its 2 permits"
        );
        drop(held);
        // Reclaimed on drop — an early return or a panic mid-handshake cannot leak a permit.
        assert!(
            gate.acquire(Duration::from_millis(500)).is_some(),
            "dropping a permit did not return it to the gate"
        );
    }

    #[test]
    fn waiting_for_a_permit_times_out_rather_than_blocking_forever() {
        // The gate is a contention reducer, not a correctness barrier: if every permit is held by
        // a stuck bridge, callers must proceed anyway rather than queue behind it indefinitely.
        let gate = test_gate(1);
        let _held = gate.acquire(Duration::from_secs(5)).unwrap();
        let t0 = Instant::now();
        assert!(gate.acquire(Duration::from_millis(100)).is_none());
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "acquire() blocked past its wait bound"
        );
    }

    #[test]
    fn the_default_start_concurrency_is_bounded_and_non_zero() {
        // Zero would make every start wait out START_WAIT before proceeding contended anyway —
        // pure dead time, since the gate is not a correctness barrier and nothing is gained by
        // the wait. Unbounded would reinstate the contention that makes `session/new` outrun its
        // budget. Asserted on `parse_permits(None)` rather than `start_permits()` so a host that
        // sets the (supported) override does not fail a test about the shipped default.
        let n = parse_permits(None);
        assert!(
            n > 0 && n <= 8,
            "default start concurrency {n} is out of range"
        );
    }

    #[test]
    fn a_start_concurrency_override_must_be_a_positive_number_or_the_default_stands() {
        // Zero is the dangerous one: a gate with no permits would make every start wait out
        // START_WAIT before proceeding contended anyway — 30s of dead time per unit.
        assert_eq!(
            parse_permits(Some("0".into())),
            START_PERMITS_DEFAULT,
            "zero"
        );
        assert_eq!(
            parse_permits(Some("two".into())),
            START_PERMITS_DEFAULT,
            "garbage"
        );
        assert_eq!(parse_permits(Some("6".into())), 6, "valid");
    }

    /// Writes a stub ACP bridge: answers `initialize` and `session/new`, and brackets its own
    /// handshake window with `+` / `-` in a shared file so the test can reconstruct how many
    /// bridges were genuinely inside a handshake at the same moment.
    #[cfg(unix)]
    fn stub_bridge(dir: &std::path::Path, ledger: &std::path::Path) -> std::path::PathBuf {
        let script = dir.join("stub-acp-bridge.sh");
        std::fs::write(
            &script,
            format!(
                r#"#!/bin/sh
printf '+' >> "{ledger}"
read _init
printf '%s\n' '{{"jsonrpc":"2.0","id":1,"result":{{}}}}'
read _new
# Hold the window open so genuinely-concurrent starts overlap in the ledger.
sleep 0.4
printf '%s\n' '{{"jsonrpc":"2.0","id":2,"result":{{"sessionId":"stub"}}}}'
printf -- '-' >> "{ledger}"
sleep 30
"#,
                ledger = ledger.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    /// The fix that matters most. `session/new` cost scales with how many bridges start at once
    /// (1.67s at K=1 → 7.12s median / 11.57s max at K=8), and a handshake that outruns its budget
    /// does not fail the unit — it silently downgrades it to ungoverned execution. Bounding the
    /// overlap is what keeps that cost off the curve.
    ///
    /// Measured through the real `start_acp_process`, not the gate in isolation, so deleting the
    /// permit acquisition fails this test rather than leaving a passing unit test behind.
    #[test]
    #[cfg(unix)]
    fn eight_simultaneous_starts_never_exceed_the_gate_in_flight() {
        let dir = std::env::temp_dir().join(format!("wicked-acp-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ledger = dir.join("overlap.txt");
        std::fs::write(&ledger, "").unwrap();
        let script = stub_bridge(&dir, &ledger);

        let config = AcpConfig {
            binary: script.to_string_lossy().to_string(),
            start_args: vec![],
            transport: AcpTransport::default(),
        };

        let handles: Vec<_> = (0..8)
            .map(|_| {
                let config = config.clone();
                let cwd = dir.clone();
                std::thread::spawn(move || start_acp_process(&config, &cwd))
            })
            .collect();
        let procs: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        // Every start must still SUCCEED. A gate that bounded contention by failing starts would
        // trade one silent downgrade for another.
        for p in &procs {
            assert!(p.is_ok(), "a gated start failed: {:?}", p.as_ref().err());
        }

        // Replay the ledger for the peak number of simultaneous handshakes.
        let marks = std::fs::read_to_string(&ledger).unwrap();
        let (mut cur, mut peak) = (0i32, 0i32);
        for c in marks.chars() {
            match c {
                '+' => {
                    cur += 1;
                    peak = peak.max(cur);
                }
                '-' => cur -= 1,
                _ => {}
            }
        }
        assert_eq!(marks.matches('+').count(), 8, "all 8 bridges ran: {marks}");
        assert!(
            peak <= start_permits() as i32,
            "peak concurrent handshakes {peak} exceeded the gate's {} permits (ledger: {marks})",
            start_permits()
        );

        drop(procs); // kills the stub children
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_bridge_that_writes_to_stderr_has_its_last_lines_kept_and_older_ones_dropped() {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg("i=1; while [ $i -le 50 ]; do echo line$i >&2; i=$((i+1)); done")
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let (tail, handle) = drain_stderr(child.stderr.take().unwrap());
        handle.join().unwrap();
        let _ = child.wait();

        let ctx = stderr_context(&tail);
        assert!(
            ctx.contains("line50"),
            "the most recent stderr line must survive: {ctx}"
        );
        assert!(
            !ctx.contains("line1 "),
            "the tail must be bounded, not the whole stream: {ctx}"
        );
        assert_eq!(
            tail.lock().unwrap().len(),
            STDERR_TAIL_LINES,
            "the tail is capped at STDERR_TAIL_LINES"
        );
    }

    #[test]
    fn appending_the_died_mid_turn_note_keeps_output_inside_its_cap() {
        // The streaming path caps `output` at MAX_OUT; this append used to run after that cap and
        // straight past it. It must fit — and the note, not the truncated stream it displaces, is
        // what an operator needs when a bridge dies mid-turn.
        let note = "\n[wicked-core] died".to_string();
        let mut at_cap = "a".repeat(100);
        append_within_cap(&mut at_cap, &note, 100);
        assert_eq!(at_cap.len(), 100, "the cap holds");
        assert!(at_cap.ends_with(&note), "the note survives the trim");

        // Room to spare: nothing is trimmed.
        let mut small = "abc".to_string();
        append_within_cap(&mut small, &note, 10_000);
        assert_eq!(small, format!("abc{note}"));

        // A multi-byte boundary at the cut point must not panic or corrupt.
        let mut wide = "é".repeat(50);
        append_within_cap(&mut wide, &note, 60);
        assert!(wide.len() <= 60);
        assert!(wide.ends_with(&note));
    }

    #[test]
    fn one_enormous_stderr_line_cannot_grow_the_tail_without_bound() {
        // A line COUNT bounds nothing on its own: a bridge that writes a megabyte and no newline
        // would sit in the tail whole, in a runner that lives as long as the daemon — and the tail
        // is appended to a capped `output`, so an unbounded line escapes that cap too.
        let huge = "x".repeat(100_000);
        let clipped = clip_stderr_line(huge);
        assert!(
            clipped.len() < STDERR_TAIL_LINE_BYTES + 64,
            "clipped to {}",
            clipped.len()
        );
        assert!(
            clipped.contains("+99488 bytes"),
            "a clipped line must say it was clipped: {clipped}"
        );

        // Multi-byte input must not be cut mid-character — `clip_stderr_line` returns a String, so
        // a bad boundary would panic rather than corrupt.
        let wide = "é".repeat(1_000);
        assert!(clip_stderr_line(wide).len() < STDERR_TAIL_LINE_BYTES + 64);

        // A line at the limit is passed through untouched.
        let small = "y".repeat(STDERR_TAIL_LINE_BYTES);
        assert_eq!(clip_stderr_line(small.clone()), small);
    }

    #[test]
    fn a_silent_bridge_is_reported_as_silent_rather_than_as_no_information() {
        // Silence is itself diagnostic — it points at contention or a hang rather than at the
        // bridge rejecting something — so it must be stated, not rendered as an empty string.
        let empty: StderrTail = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        assert!(stderr_context(&empty).contains("silent"));
    }

    #[test]
    fn chat_pool_is_isolated_from_run_sessions_and_close_is_idempotent() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        // Poisoned run entry + poisoned chat entry (None = failed start; constructing a real
        // AcpProcess needs a live child, so pool-shape tests use the None slot).
        {
            let mut guard = r.sessions.lock().unwrap();
            guard.insert(("run1".into(), "claude".into()), None);
            guard.insert((AcpStepRunner::chat_pool_key("c1"), "claude".into()), None);
        }
        // Seats lists only WARM (Some) sessions — poisoned slots are not seats.
        assert!(r.chat_seats("c1").is_empty());
        // Dropping a RUN's sessions must not touch the chat pool, and vice versa.
        r.drop_session("run1");
        assert_eq!(r.sessions.lock().unwrap().len(), 1);
        r.chat_close("c1", ChatCloseReason::Requested);
        assert_eq!(r.sessions.lock().unwrap().len(), 0);
        r.chat_close("c1", ChatCloseReason::Requested); // idempotent
                                                        // Both closes emitted ChatClosed through
                                                        // the actor emit point, carrying the
                                                        // reason the caller asked for.
        let evs: Vec<_> = rx.try_iter().collect();
        let closed = evs
            .iter()
            .filter(|c| {
                matches!(c, Command::EmitEvent(CoreEvent::ChatClosed { chat, reason })
                    if chat == "c1" && reason == "requested")
            })
            .count();
        assert_eq!(closed, 2);
    }

    /// The TTL these tests reap against. Deliberately small: `Instant` is monotonic-since-boot, so
    /// every backdate below has to be representable on a host that just booted. Keeping the whole
    /// scale within `MAX_BACKDATE` seconds means these tests never depend on machine uptime.
    const TEST_TTL: Duration = Duration::from_secs(4);
    const MAX_BACKDATE: u64 = 10;

    /// An `Instant` `secs` in the past.
    ///
    /// `checked_sub` rather than `-`: subtracting past the start of the monotonic clock panics, and
    /// a bare panic here would read as a reaper bug rather than as a host with less uptime than the
    /// backdate. Every caller stays under `MAX_BACKDATE`, so the expect is unreachable in practice.
    fn backdated(secs: u64) -> Instant {
        debug_assert!(secs <= MAX_BACKDATE, "keep test backdates small");
        Instant::now()
            .checked_sub(Duration::from_secs(secs))
            .expect("monotonic clock older than the backdate (host uptime under 10s?)")
    }

    /// Put `chat_id` in the pool and backdate its last-touch by `idle` seconds.
    ///
    /// Backdating beats sleeping: the reaper's whole contract is about elapsed time, and a test
    /// that slept for it would be both slow and flaky. Constructing a real `AcpProcess` needs a
    /// live child, so — as in the pool-shape test above — the slot is `None`; `chat_list` counts
    /// pool ENTRIES, which is what the reaper acts on.
    fn seed_chat(r: &AcpStepRunner, chat_id: &str, idle: u64) {
        r.sessions.lock().unwrap().insert(
            (AcpStepRunner::chat_pool_key(chat_id), "claude".into()),
            None,
        );
        r.chat_activity
            .lock()
            .unwrap()
            .insert(chat_id.to_string(), backdated(idle));
    }

    fn closed_chats(rx: &std::sync::mpsc::Receiver<Command>) -> Vec<(String, String)> {
        rx.try_iter()
            .filter_map(|c| match c {
                Command::EmitEvent(CoreEvent::ChatClosed { chat, reason }) => Some((chat, reason)),
                _ => None,
            })
            .collect()
    }

    /// The core of FINDING-027: nothing ever reclaimed an abandoned chat, so ~520 MB per seat
    /// stayed pinned for the daemon's lifetime. A chat past the TTL must go; one inside it must
    /// not, or an operator loses a session they are still using.
    #[test]
    fn idle_chats_are_reaped_and_active_ones_are_left_alone() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "stale", MAX_BACKDATE);
        seed_chat(&r, "fresh", 0);

        let reaped = r.chat_reap_idle(TEST_TTL);

        assert_eq!(reaped, vec!["stale".to_string()]);
        assert_eq!(
            r.chat_list().iter().map(|c| &c.chat_id).collect::<Vec<_>>(),
            vec!["fresh"],
            "the chat inside its TTL must survive"
        );
        assert_eq!(
            closed_chats(&rx),
            vec![("stale".to_string(), "idle".to_string())],
            "a reclaim must be distinguishable from an operator's own close"
        );
    }

    /// A touch is what proves a chat is still in use, and `chat_ensure` is the funnel every use
    /// passes through (`chat_turn` calls it too). Without the touch there, a chat being actively
    /// talked to would age out mid-conversation.
    ///
    /// Driven through the FAILING ensure path deliberately: it is the one reachable without a live
    /// child, and it pins the stronger claim — the touch is unconditional, so a chat mid-warm-up
    /// is never mistaken for an abandoned one by a reaper running concurrently.
    #[test]
    fn ensuring_a_seat_touches_the_chat_even_when_the_seat_fails_to_start() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "c1", MAX_BACKDATE);

        assert!(r
            .chat_ensure("c1", "no-such-cli-xyz", &std::env::temp_dir())
            .is_err());

        assert!(
            r.chat_reap_idle(TEST_TTL).is_empty(),
            "a chat someone just tried to warm a seat on is not idle"
        );
    }

    /// The TTL cannot cover chats opened faster than it retires them. The cap is the backstop —
    /// and it must evict the LEAST recently used, never the one being opened.
    #[test]
    fn the_pool_cap_evicts_least_recently_used_and_never_the_newest() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        for (id, idle) in [("oldest", 3), ("middle", 2), ("newer", 1), ("newest", 0)] {
            seed_chat(&r, id, idle);
        }

        let evicted = r.chat_enforce_cap(2);

        assert_eq!(evicted, vec!["oldest".to_string(), "middle".to_string()]);
        assert_eq!(
            r.chat_list().iter().map(|c| &c.chat_id).collect::<Vec<_>>(),
            vec!["newer", "newest"]
        );
        let reasons: Vec<String> = closed_chats(&rx).into_iter().map(|(_, r)| r).collect();
        assert_eq!(reasons, vec!["pool_cap".to_string(); 2]);
    }

    /// `WICKED_CHAT_POOL_MAX=0` must not mean "evict everything the instant it opens". A pool that
    /// cannot hold the chat being opened is not a smaller pool, it is a broken one.
    #[test]
    fn a_zero_pool_cap_is_floored_at_one_rather_than_evicting_everything() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "only", 0);

        assert!(r.chat_enforce_cap(0).is_empty());
        assert_eq!(r.chat_list().len(), 1);
    }

    /// Closing must drop the activity entry too. Chat ids are minted by clients without bound, so
    /// a map that only ever grows trades a 520 MB leak for a slower one.
    #[test]
    fn closing_a_chat_forgets_its_activity() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        seed_chat(&r, "c1", 10);

        r.chat_close("c1", ChatCloseReason::Requested);

        assert!(r.chat_activity.lock().unwrap().is_empty());
        assert!(r.chat_list().is_empty());
    }

    /// A turn outliving the TTL re-touches on its way out, AFTER the reaper has already closed the
    /// chat — leaving an activity entry `chat_close` cannot collect because it ran first. The
    /// sweep must collect it, or the daemon trades a 520 MB leak for a slow unbounded one.
    ///
    /// And it must NOT collect an entry that is merely unpooled-so-far: `chat_ensure` touches
    /// before it inserts, so a chat mid-open looks exactly like an orphan for an instant.
    #[test]
    fn the_sweep_collects_stale_orphan_activity_but_spares_a_chat_mid_open() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        {
            let mut activity = r.chat_activity.lock().unwrap();
            // Re-touched by a long turn after its chat was already closed.
            activity.insert("orphan".to_string(), backdated(MAX_BACKDATE));
            // Touched by `chat_ensure`, whose pool insert has not landed yet.
            activity.insert("opening".to_string(), Instant::now());
        }

        r.chat_reap_idle(TEST_TTL);

        let remaining: Vec<String> = r.chat_activity.lock().unwrap().keys().cloned().collect();
        assert_eq!(
            remaining,
            vec!["opening".to_string()],
            "the stale orphan goes, the chat mid-open stays"
        );
    }

    /// A pool entry with no recorded activity must read as idle-since-forever, not as fresh.
    /// The conservative reading reclaims it; the other one would let any gap in touch-recording
    /// pin memory permanently — which is exactly the defect being fixed.
    #[test]
    fn a_chat_with_no_recorded_activity_is_reaped_rather_than_treated_as_fresh() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        r.sessions.lock().unwrap().insert(
            (AcpStepRunner::chat_pool_key("orphan"), "claude".into()),
            None,
        );

        assert_eq!(r.chat_list()[0].idle_secs, u64::MAX);
        assert_eq!(r.chat_reap_idle(TEST_TTL), vec!["orphan".to_string()]);
    }

    /// The enumerate surface (FINDING-027 gap 4): a leak nobody can list is a leak nobody can
    /// reclaim. Two seats of one chat collapse to ONE entry, and a run's sessions are not chats.
    #[test]
    fn chat_list_collapses_a_chats_seats_and_ignores_run_sessions() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        {
            let mut guard = r.sessions.lock().unwrap();
            guard.insert(("run1".into(), "claude".into()), None);
            guard.insert((AcpStepRunner::chat_pool_key("c1"), "codex".into()), None);
            guard.insert((AcpStepRunner::chat_pool_key("c1"), "claude".into()), None);
        }
        r.chat_touch("c1");

        let listed = r.chat_list();

        assert_eq!(listed.len(), 1, "a run session is not a chat: {listed:?}");
        assert_eq!(listed[0].chat_id, "c1");
        assert!(listed[0].idle_secs < 5);
        // `seats` is the WARM subset, and these slots are `None` — a real `AcpProcess` needs a
        // live child, so unit tests cannot produce one. The seat-name path is covered by
        // `chat_seats`, which reads the same map with the same warm filter.
        assert!(listed[0].seats.is_empty());
    }

    #[test]
    fn chat_ensure_fails_loud_for_unknown_cli_and_does_not_poison() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        let cwd = std::env::temp_dir();
        let err = match r.chat_ensure("c1", "no-such-cli-xyz", &cwd) {
            Err(e) => e,
            Ok(_) => panic!("unknown cli must fail"),
        };
        assert!(err.contains("no ACP config"), "{err}");
        // Chat failures are retryable — nothing cached, seat list stays empty.
        assert!(r.chat_seats("c1").is_empty());
        assert!(r.sessions.lock().unwrap().is_empty());
    }

    #[test]
    fn queued_messages_deliver_to_matching_cli_and_stay_for_others() {
        let r = runner();
        assert!(r.queue_operator_message("run1", &InjectTarget::All, "for everyone"));
        assert!(r.queue_operator_message("run1", &InjectTarget::Cli("codex".into()), "codex only"));
        assert!(r.queue_operator_message("run1", &InjectTarget::Cli("agy".into()), "agy only"));

        // claude drains the broadcast but not the CLI-targeted entries; the delivery
        // record carries the ORIGINAL injection target, not the receiving CLI.
        let claude = r.drain_operator_messages("run1", "claude");
        assert_eq!(claude.len(), 1);
        assert_eq!(claude[0].0, "all");
        assert_eq!(claude[0].1.output, "for everyone");
        assert_eq!(claude[0].1.label, "[operator message]");

        // codex drains only its own targeted entry (broadcast already consumed).
        let codex = r.drain_operator_messages("run1", "codex");
        assert_eq!(codex.len(), 1);
        assert_eq!(codex[0].0, "codex");
        assert_eq!(codex[0].1.output, "codex only");

        // agy's entry survived both prior drains.
        let agy = r.drain_operator_messages("run1", "agy");
        assert_eq!(agy.len(), 1);
        assert_eq!(agy[0].1.output, "agy only");

        // Everything consumed; nothing left for anyone.
        assert!(r.drain_operator_messages("run1", "claude").is_empty());
    }

    #[test]
    fn result_usage_parses_ecosystem_adapter_shape() {
        // Official claude adapter result: input + cached reads/writes sum into input.
        let v = serde_json::json!({
            "inputTokens": 2, "outputTokens": 4,
            "cachedReadTokens": 15273, "cachedWriteTokens": 18195, "totalTokens": 33474
        });
        let u = parse_result_usage(&v).expect("usage");
        assert_eq!(u.input_tokens, 2 + 15273 + 18195);
        assert_eq!(u.output_tokens, 4);
        assert_eq!(u.cost_usd, None);

        // Absent / empty / zeroed → None (no fabricated usage).
        assert!(parse_result_usage(&serde_json::Value::Null).is_none());
        assert!(parse_result_usage(&serde_json::json!({})).is_none());
        assert!(
            parse_result_usage(&serde_json::json!({"inputTokens": 0, "outputTokens": 0})).is_none()
        );
    }

    #[test]
    fn usage_update_lifts_cost_only_frames() {
        // Official claude adapter usage_update: {used, size, cost:{amount}} — no token
        // fields. The cost must be lifted and must survive a later result-usage merge.
        let emit_fn = |_: &str| {};
        let emit: &DeltaSink = &emit_fn;
        let mut output = String::new();
        let mut usage: Option<Usage> = None;
        let mut files = Vec::new();
        let v = serde_json::json!({
            "params": {"update": {
                "sessionUpdate": "usage_update",
                "used": 33474, "size": 1000000,
                "cost": {"amount": 0.19, "currency": "USD"}
            }}
        });
        handle_update(&v, emit, &mut output, &mut usage, &mut files, 1024);
        let u = usage.expect("cost-only frame lifts usage");
        assert_eq!(u.cost_usd, Some(0.19));
        assert_eq!(u.input_tokens, 0);

        // Merge semantics from the turn loop: result usage wins tokens, keeps cost.
        let result_usage = parse_result_usage(&serde_json::json!({
            "inputTokens": 10, "outputTokens": 5
        }))
        .unwrap();
        let cost = u.cost_usd;
        let merged = Usage {
            cost_usd: cost.or(result_usage.cost_usd),
            ..result_usage
        };
        assert_eq!(merged.input_tokens, 10);
        assert_eq!(merged.output_tokens, 5);
        assert_eq!(merged.cost_usd, Some(0.19));
    }

    #[test]
    fn drain_is_scoped_per_run_and_drop_session_prunes() {
        let r = runner();
        assert!(r.queue_operator_message("run1", &InjectTarget::All, "run1 msg"));
        assert!(r.queue_operator_message("run2", &InjectTarget::All, "run2 msg"));

        // run2's queue is untouched by run1's drain.
        assert_eq!(r.drain_operator_messages("run1", "claude").len(), 1);
        assert_eq!(r.drain_operator_messages("run1", "claude").len(), 0);

        // drop_session prunes the run's queue outright.
        r.drop_session("run2");
        assert!(r.drain_operator_messages("run2", "claude").is_empty());
    }

    // ── FINDING-060/061: a governed claude unit must never run on ACP ────────────

    /// What the fallback actually invokes, per platform.
    ///
    /// The routing this test exists for is platform-independent, so the test itself is NOT
    /// `#[cfg]`-gated — per the argument at `execute_wrapped.rs`'s `rule_path_sep`, a gated test
    /// runs on one of three CI platforms, which is how a platform bug survives review. Only the
    /// *execution* proof needs a real process, and only that assertion is gated.
    ///
    /// There is no Windows equivalent of `/bin/echo` here, and `cmd /c echo` is not one:
    /// `build_argv` appends the skill prompt as a trailing arg whenever the template omits
    /// `{PROMPT}` (execute_wrapped.rs:1228), and that prompt carries `|||` and newlines — pipes and
    /// command separators to `cmd`. So Windows names a binary that cannot exist: the spawn fails
    /// fast, without a shell, and every assertion below except the execution proof still holds,
    /// because `fallback_with_warning` prepends its warning whether or not the child runs.
    #[cfg(unix)]
    const CHEAP_OK: &str = "/bin/echo wicked-fallback-ran";
    #[cfg(not(unix))]
    const CHEAP_OK: &str = "wicked-no-such-binary-fallback-probe";

    /// A unit assigned to `claude` — the routing predicate reads `assigned_cli` — whose actual
    /// invocation is [`CHEAP_OK`], so the wrapped fallback this must reach executes something cheap
    /// instead of a real CLI. The two are deliberately different: the ACP branch classifies by the
    /// assigned key, the wrapped runner by argv[0].
    fn claude_unit_running_echo() -> crate::domain::WorkUnit {
        crate::domain::WorkUnit {
            id: "u-gov".to_string(),
            session_id: "run-gov".to_string(),
            ord: 1,
            description: "a governed unit".to_string(),
            stage: Default::default(),
            assigned_cli: Some("claude".to_string()),
            assigned_invocation: Some(CHEAP_OK.to_string()),
            council_task_ref: None,
            routing: None,
            denial_reason: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            skill_ref: None,
            allowed_skills: Vec::new(),
            gate: Default::default(),
            role: Default::default(),
            validator: None,
            tool_cmd: None,
            depends_on: Vec::new(),
            status: crate::domain::UnitStatus::Pending,
        }
    }

    fn governed_input(dir: &std::path::Path) -> StepInput {
        StepInput {
            run_id: "run-gov".to_string(),
            unit_ix: 0,
            attempt: 0,
            unit: claude_unit_running_echo(),
            workflow_id: "wf-test".to_string(),
            entity_mode: crate::scope::EntityMode::Shared,
            workdir: Some(dir.to_path_buf()),
            governance: Some(crate::workflow::GovernanceContext {
                db_path: dir.join("estate.db").to_string_lossy().to_string(),
                code_graph_db: None,
            }),
            prior_outputs: Vec::new(),
        }
    }

    /// The regression this exists for: the ACP path armed governance the bridge never applied, so a
    /// governed unit ran with every tool call ungoverned while the engine reported `governed: true`
    /// (FINDING-060). The fix routes governed claude units to the wrapped path — the only one where
    /// the gate-hook is measured to fire — and it has to hold *before* any ACP session is opened,
    /// which is what the absence of an ACP-session event below pins.
    #[test]
    fn a_governed_claude_unit_leaves_the_acp_path_before_a_session_is_opened() {
        let (tx, rx) = std::sync::mpsc::channel();
        let r = AcpStepRunner::new(tx);
        let dir = std::env::temp_dir().join(format!(
            "wicked-acpgov-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        let noop = |_: &str| {};
        let out = r.run_unit_streaming(&governed_input(&dir), &noop);

        let events: Vec<CoreEvent> = rx
            .try_iter()
            .filter_map(|c| match c {
                Command::EmitEvent(e) => Some(e),
                _ => None,
            })
            .collect();

        let fallback = events
            .iter()
            .find_map(|e| match e {
                CoreEvent::AcpFallback {
                    cli_key,
                    fallback_kind,
                    reason,
                    ..
                } => Some((cli_key.clone(), fallback_kind.clone(), reason.clone())),
                _ => None,
            })
            .expect("the reroute must be announced, not silent — an unobserved path change is how the ungoverned ACP path went unnoticed");
        assert_eq!(fallback.0, "claude");
        assert_eq!(fallback.1, fallback_kind::GOVERNANCE_REQUIRES_WRAPPED);
        assert!(fallback.2.contains("does not apply"), "{}", fallback.2);

        // No ACP session was opened for this run: the branch returns before `start_acp_process`.
        assert!(
            r.sessions
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .is_empty(),
            "a governed unit must not reach the ACP session pool at all"
        );
        // The downgrade has to be legible where a human reads the run, not only in the event
        // stream: every other ACP fallback prepends its `[wicked-core] …` warning to the output,
        // and a governed unit silently losing multi-turn is the one that most needs to say so.
        assert!(
            out.output.contains("[wicked-core]") && out.output.contains("does not apply"),
            "the reroute warning must be prepended to StepOutput.output: {}",
            out.output
        );

        // Execution proof — the reroute reached a runner that ran the invocation, rather than
        // short-circuiting into a synthetic result. Needs a real echo, so unix only ([`CHEAP_OK`]).
        #[cfg(unix)]
        {
            assert_eq!(out.status, StepStatus::Ok, "output: {}", out.output);
            assert!(
                out.output.contains("wicked-fallback-ran"),
                "the wrapped runner must have actually run the invocation: {}",
                out.output
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The predicate the branch turns on, at its boundary. `cli_runs_claude` classifies by the
    /// REGISTERED BINARY, so a seat whose key is not `claude` still routes to wrapped when its
    /// binary is — and a non-claude seat keeps the shared ACP path, since input arming was always
    /// claude-only and rerouting it would cost multi-turn for nothing.
    #[test]
    fn the_reroute_predicate_follows_the_binary_not_the_key() {
        assert!(cli_runs_claude("claude"));
        // Ad-hoc keys not in the registry classify on the key itself, path-stem-wise.
        assert!(cli_runs_claude("/opt/somewhere/claude"));
        assert!(!cli_runs_claude("codex"));
        assert!(!cli_runs_claude("claude-ish"));
    }
}
