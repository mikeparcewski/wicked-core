//! ACP (Agent Client Protocol) session runner — multi-CLI extension of wicked-core#13.
//!
//! Drives persistent multi-turn sessions using the standardised JSON-RPC 2.0 ndjson
//! (stdin/stdout) ACP protocol. Each CLI runs its own ACP wrapper binary; wicked-core
//! is the ACP client. The registry maps CLI keys to their ACP binary:
//!
//! | CLI      | ACP binary        | Transport |
//! |----------|-------------------|-----------|
//! | claude   | claude-agent-acp  | stdio     |
//! | agy      | agy-acp           | stdio     |
//! | codex    | codex-acp         | stdio     |
//! | pi       | pi-acp            | stdio     |
//! | copilot  | copilot --acp     | HTTP      |
//!
//! When an ACP binary is unavailable or fails during the handshake, `AcpStepRunner`
//! emits a warning in the step output and falls back to [`WrappedCliStepRunner`]
//! (single-shot invocation). HTTP transport is not yet implemented; copilot falls back
//! gracefully until it is.
//!
//! # Session lifecycle
//! - **Open (lazy)**: on the first unit for a `run_id`, the binary is spawned and the
//!   `initialize` + `session/new` JSON-RPC handshake completes.
//! - **Reuse**: subsequent units send `session/prompt` to the same process and stream
//!   `session/update` text chunks until `stopReason` arrives — sharing prompt-cache
//!   across governance turns without a per-unit cold start.
//! - **Close**: [`AcpStepRunner::drop_session`] kills the process. Call it from the
//!   caller after a run's last unit (mirrors [`PersistentStepRunner::drop_session`]).
//!
//! # Protocol
//! JSON-RPC 2.0 ndjson over stdin/stdout:
//! - `initialize` → agent returns capabilities.
//! - `session/new` → agent returns `sessionId`.
//! - `session/prompt` → agent streams `session/update` notifications then responds
//!   with `{ "result": { "stopReason": "end_turn" } }`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::execute_wrapped::{skill_prompt, WrappedCliStepRunner};
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus, Usage};
use wicked_council::types::{AcpConfig, AcpTransport};

// ── ACP child process ─────────────────────────────────────────────────────────

struct AcpProcess {
    child: Child,
    stdin: BufWriter<ChildStdin>,
    /// Lines arriving from the ACP server's stdout, sent by the reader thread.
    line_rx: std::sync::mpsc::Receiver<String>,
    /// Keeps the reader thread alive until the process is dropped.
    _reader: std::thread::JoinHandle<()>,
    session_id: String,
    next_id: u64,
}

impl Drop for AcpProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

// ── Session startup ───────────────────────────────────────────────────────────

/// Spawn the ACP binary and complete the `initialize` + `session/new` handshake.
/// Returns `Err` if the binary is not found, the process fails to start, or the
/// handshake does not complete within 10 s.
fn start_acp_process(config: &AcpConfig, cwd: &std::path::Path) -> anyhow::Result<AcpProcess> {
    let mut cmd = std::process::Command::new(&config.binary);
    cmd.args(&config.start_args);
    cmd.current_dir(cwd);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("ACP binary '{}': {e}", config.binary))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("ACP binary '{}': no stdout", config.binary))?;
    let mut stdin = BufWriter::new(
        child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("ACP binary '{}': no stdin", config.binary))?,
    );

    // Spawn a reader thread that feeds lines to the mpsc channel so the caller can
    // use recv_timeout without blocking the whole thread on a slow read.
    let (tx, rx) = std::sync::mpsc::sync_channel(512);
    let reader_thread = std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if !line.is_empty() && tx.send(line).is_err() {
                break;
            }
        }
    });

    const HANDSHAKE: Duration = Duration::from_secs(10);

    rpc_send(
        &mut stdin,
        1,
        "initialize",
        json!({
            "protocolVersion": 1,
            "clientCapabilities": {"fs": {}, "terminal": false},
            "clientInfo": {"name": "wicked-core", "version": env!("CARGO_PKG_VERSION")}
        }),
    )?;
    rpc_expect(&rx, 1, HANDSHAKE)?;

    rpc_send(
        &mut stdin,
        2,
        "session/new",
        json!({
            "cwd": cwd.to_string_lossy().as_ref()
        }),
    )?;
    let resp = rpc_expect(&rx, 2, HANDSHAKE)?;
    let session_id = resp["result"]["sessionId"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("ACP session/new: missing sessionId in response"))?
        .to_string();

    Ok(AcpProcess {
        child,
        stdin,
        line_rx: rx,
        _reader: reader_thread,
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

/// Wait for the JSON-RPC response whose `"id"` matches `id`, skipping notifications.
/// Returns `Err` on timeout, disconnect, or a server-side `"error"` field.
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
        match rx.recv_timeout(remaining.min(Duration::from_millis(100))) {
            Ok(line) => {
                let v: Value = serde_json::from_str(&line)
                    .map_err(|e| anyhow::anyhow!("ACP non-JSON line during handshake: {e}"))?;
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
fn exec_turn_acp(
    proc: &mut AcpProcess,
    prompt: &str,
    emit: &DeltaSink,
    timeout: Duration,
) -> anyhow::Result<TurnResult> {
    let id = proc.next_id;
    proc.next_id += 1;

    rpc_send(
        &mut proc.stdin,
        id,
        "session/prompt",
        json!({
            "sessionId": proc.session_id,
            "prompt": [{"type": "text", "text": prompt}]
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
        match proc
            .line_rx
            .recv_timeout(remaining.min(Duration::from_millis(100)))
        {
            Ok(line) => {
                let v: Value = match serde_json::from_str(&line) {
                    Ok(v) => v,
                    Err(_) => continue, // skip non-JSON noise (startup banners, etc.)
                };

                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    // Response to our session/prompt request — turn complete.
                    // stopReason lives in result; non-"cancelled" values map to Ok.
                    let stop = v["result"]["stopReason"].as_str().unwrap_or("end_turn");
                    if stop == "cancelled" {
                        timed_out = true;
                    } else {
                        found = true;
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
    _files: &mut Vec<String>,
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
                if output.len() < max_out {
                    output.push_str(text);
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
            if input > 0 || out > 0 {
                *usage = Some(Usage {
                    input_tokens: input,
                    output_tokens: out,
                    cost_usd: None,
                });
            }
        }
        _ => {}
    }
}

// ── AcpStepRunner ─────────────────────────────────────────────────────────────

/// A [`StepRunner`] that drives ACP multi-turn sessions for multi-CLI support.
///
/// Consults the built-in CLI registry for each CLI key to find the ACP wrapper binary.
/// Falls back to single-shot [`WrappedCliStepRunner`] when:
/// - the CLI has no ACP config in the registry
/// - the ACP binary is not on PATH
/// - the handshake fails
/// - the ACP session dies mid-run
///
/// Each fallback emits a `[wicked-core] ACP …` warning line in the step output so
/// callers can distinguish ACP paths from fallback paths.
pub struct AcpStepRunner {
    sessions: Arc<Mutex<HashMap<String, Arc<Mutex<AcpProcess>>>>>,
    fallback: WrappedCliStepRunner,
    timeout: Duration,
}

impl AcpStepRunner {
    pub(crate) fn new() -> Self {
        let secs = std::env::var("WICKED_UNIT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(7200);
        Self {
            sessions: Arc::new(Mutex::new(HashMap::new())),
            fallback: WrappedCliStepRunner::default(),
            timeout: Duration::from_secs(secs),
        }
    }

    /// Close the ACP session for `run_id` and kill its child process. Idempotent.
    /// Call this after the last unit of a run completes (mirrors
    /// [`PersistentStepRunner::drop_session`]).
    pub fn drop_session(&self, run_id: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(run_id);
        // Dropping the Arc<Mutex<AcpProcess>> eventually drops AcpProcess::drop → kill.
    }

    fn exec_turn(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        let run_id = input.run_id.clone();
        let cli_key = input.unit.assigned_cli.as_deref().unwrap_or("claude");

        // Resolve the ACP config for this CLI from the built-in registry.
        let acp_config = match acp_config_for(cli_key) {
            Some(c) => c,
            None => return self.fallback.run_unit_streaming(input, emit),
        };

        // HTTP transport is not yet implemented; fall back gracefully.
        if acp_config.transport == AcpTransport::Http {
            let msg = format!(
                "[wicked-core] ACP HTTP transport not yet implemented for '{cli_key}'; \
                 using single-shot fallback"
            );
            emit(&msg);
            return self.fallback.run_unit_streaming(input, emit);
        }

        // Lazily open a session for this run. Holds the global map lock only for the
        // short map lookup/insert — not across the blocking spawn + handshake.
        let proc_arc: Arc<Mutex<AcpProcess>> = {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(arc) = guard.get(&run_id) {
                arc.clone()
            } else {
                drop(guard);
                let cwd = input
                    .workdir
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                match start_acp_process(&acp_config, &cwd) {
                    Ok(proc) => {
                        let arc = Arc::new(Mutex::new(proc));
                        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                        // entry().or_insert handles concurrent openers: the first one wins.
                        guard
                            .entry(run_id.clone())
                            .or_insert_with(|| arc.clone())
                            .clone()
                    }
                    Err(e) => {
                        let msg = format!(
                            "[wicked-core] ACP unavailable for '{cli_key}' ({e}); \
                             using single-shot fallback"
                        );
                        emit(&msg);
                        return self.fallback.run_unit_streaming(input, emit);
                    }
                }
            }
        };

        let mut proc = proc_arc.lock().unwrap_or_else(|p| p.into_inner());
        let prompt = skill_prompt(&input.unit);

        match exec_turn_acp(&mut proc, &prompt, emit, self.timeout) {
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
                // Deadline elapsed — process is still alive; don't fall back.
                drop(proc);
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
                // ACP process exited or returned Failed — drop the stale session.
                drop(proc);
                self.drop_session(&run_id);
                let msg = format!(
                    "[wicked-core] ACP session exited for '{cli_key}'; using single-shot fallback"
                );
                emit(&msg);
                self.fallback.run_unit_streaming(input, emit)
            }
            Err(e) => {
                drop(proc);
                self.drop_session(&run_id);
                let msg = format!(
                    "[wicked-core] ACP error for '{cli_key}' ({e}); using single-shot fallback"
                );
                emit(&msg);
                self.fallback.run_unit_streaming(input, emit)
            }
        }
    }
}

impl StepRunner for AcpStepRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let noop = |_: &str| {};
        self.exec_turn(input, &noop)
    }

    fn run_unit_streaming(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        self.exec_turn(input, emit)
    }
}

// ── Registry helper ───────────────────────────────────────────────────────────

/// Look up the ACP config for a CLI key from the built-in registry.
fn acp_config_for(cli_key: &str) -> Option<AcpConfig> {
    wicked_council::registry::builtin()
        .into_iter()
        .find(|c| c.key == cli_key)
        .and_then(|c| c.acp)
}
