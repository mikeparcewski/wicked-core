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
use crate::execute_wrapped::{skill_prompt, WrappedCliStepRunner};
use crate::workflow::{
    DeltaSink, GovernanceContext, PriorUnitOutput, StepInput, StepOutput, StepRunner, StepStatus,
    Usage,
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
/// Returns `Err` if the binary is not on PATH, the process fails to start, or the
/// handshake does not complete within 10 s.
///
/// When `gov` is `Some`, `--settings <path>` is prepended to the binary's argv (before
/// `config.start_args`) so Claude's PreToolUse gate-hook fires on every tool call, and the
/// governance env vars are set on the child process so the hook subprocess can locate the
/// decisions log, store, scope, and phase. This is the ACP equivalent of what
/// `execute_wrapped::arm_input_governance` does for the single-shot wrapped-CLI path.
fn start_acp_process(
    config: &AcpConfig,
    cwd: &std::path::Path,
    gov: Option<&AcpGovArmed>,
) -> anyhow::Result<AcpProcess> {
    let build_cmd = |binary: &str| {
        let mut cmd = std::process::Command::new(binary);
        // --settings must precede all other start_args so it is parsed as a flag, not a
        // positional argument. Mirrors arm_input_governance's position-1 insertion.
        if let Some(g) = gov {
            cmd.arg("--settings").arg(&g.settings_path);
            cmd.env(crate::gate_hook::DECISIONS_PATH_ENV, &g.decisions_path);
            cmd.env(crate::gate_hook::ESTATE_DB_ENV, &g.db_path);
            cmd.env(crate::gate_hook::GATE_SCOPE_ENV, &g.scope);
            cmd.env(crate::gate_hook::GATE_PHASE_ENV, &g.phase);
            if !g.phase_id.is_empty() {
                cmd.env(crate::gate_hook::GATE_PHASE_ID_ENV, &g.phase_id);
            }
        }
        cmd.args(&config.start_args);
        cmd.current_dir(cwd);
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::null());
        cmd
    };

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
    // the background process when initialize/session-new fails or times out.
    macro_rules! handshake_err {
        ($child:expr, $e:expr) => {{
            let _ = $child.kill();
            let _ = $child.wait();
            return Err($e);
        }};
    }

    const HANDSHAKE: Duration = Duration::from_secs(10);

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
    if let Err(e) = rpc_expect(&rx, 1, HANDSHAKE) {
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
    let resp = match rpc_expect(&rx, 2, HANDSHAKE) {
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
/// `prior_outputs` are injected as leading ACP prompt blocks so the agent sees what peer CLIs
/// produced before this turn. For a single-CLI run this slice is empty; the prompt stays
/// a single text block exactly as before. For multi-CLI runs each prior block is prefixed with
/// its label so the agent can attribute each peer's contribution.
fn exec_turn_acp(
    proc: &mut AcpProcess,
    prompt: &str,
    prior_outputs: &[PriorUnitOutput],
    emit: &DeltaSink,
    timeout: Duration,
) -> anyhow::Result<TurnResult> {
    let id = proc.next_id;
    proc.next_id += 1;

    // Build the prompt block array: prior-CLI context (if any) followed by the work prompt.
    let mut blocks: Vec<Value> = prior_outputs
        .iter()
        .map(|p| {
            json!({
                "type": "text",
                "text": format!("{}\n{}", p.label, p.output)
            })
        })
        .collect();
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

/// Quote the current-exe path for the platform's shell so `$`/backtick/space in the install
/// path can't be expanded or split. Mirrors `execute_wrapped`'s private `quote_exe_command`.
fn quote_exe_for_hook(exe: &str) -> String {
    #[cfg(unix)]
    {
        format!("'{}' gate-hook", exe.replace('\'', "'\\''"))
    }
    #[cfg(not(unix))]
    {
        format!("\"{exe}\" gate-hook")
    }
}

/// Everything the ACP launcher needs to propagate input governance into the ACP subprocess.
/// Returned by [`arm_acp_governance`]; the settings file and armed marker have already been
/// written to disk when this is returned.
struct AcpGovArmed {
    /// Absolute path to the per-(unit, attempt) `--settings` JSON file.
    settings_path: std::path::PathBuf,
    /// Absolute path to the append-only decisions NDJSON (set as `WICKED_DECISIONS_PATH`).
    decisions_path: std::path::PathBuf,
    /// The estate SQLite store path (set as `WICKED_ESTATE_DB` on the ACP child process).
    db_path: String,
    /// The unit's collection scope, e.g. `wicked-agent/<run>/unit/<id>` (set as
    /// `WICKED_GATE_SCOPE`).
    scope: String,
    /// The unit's orchestration phase, e.g. `unit-3` (set as `WICKED_GATE_PHASE`).
    phase: String,
    /// The unit's WORKFLOW phase id, e.g. `review` (set as `WICKED_GATE_PHASE_ID`) so the hook's
    /// policy `select` also matches an operator-authored `applies_to` (FINDING-021). Empty ⇒ unset.
    phase_id: String,
}

/// Arm input governance for a governed ACP session. Produces and writes the per-(unit, attempt)
/// `--settings` JSON file declaring the `PreToolUse` gate-hook, writes the ARMED marker to the
/// decisions log, and returns the [`AcpGovArmed`] the caller uses to configure the ACP process.
///
/// Mirrors `execute_wrapped::arm_input_governance` (for the wrapped-CLI path) exactly:
/// - same settings JSON structure (hook command, matcher, type)
/// - same decisions-log path derivation ([`gate_hook::decisions_path_for`])
/// - same O_EXCL + unlink-on-clash file write (closes TOCTOU)
/// - same armed-marker write (evidence-integrity)
///
/// SECURITY: only the trusted `current_exe()` is interpolated into the hook command string.
/// Scope/phase (which embed the caller-controlled `session_id`/`unit.id`) travel via ENV, not
/// the command string, so no attacker-controlled data ever reaches the shell-executed hook.
fn arm_acp_governance(input: &StepInput, gov: &GovernanceContext) -> std::io::Result<AcpGovArmed> {
    let scope = crate::scope::resolve_scope(input.entity_mode, &input.run_id, &input.unit.id);
    let phase = crate::scope::unit_phase(input.unit.ord);
    let decisions_path = crate::gate_hook::decisions_path_for(&input.run_id, input.attempt);
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "wicked-core".to_string());
    let command = quote_exe_for_hook(&exe);
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "*", "hooks": [ { "type": "command", "command": command } ] }
            ]
        }
    });
    let dir = decisions_path
        .parent()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("wicked-core-gov"));
    crate::gate_hook::create_dir_all_private(&dir)?;
    // Per-unit settings file written with O_EXCL; clash → unlink the existing entry (a symlink
    // is unlinked itself, never followed) then re-create — same TOCTOU mitigation as the
    // wrapped-CLI path (execute_wrapped::arm_input_governance).
    let settings_path = dir.join(format!("settings-{phase}.json"));
    let bytes = serde_json::to_vec(&settings).map_err(std::io::Error::other)?;
    {
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&settings_path)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    let _ = std::fs::remove_file(&settings_path);
                    std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .open(&settings_path)
                } else {
                    Err(e)
                }
            })?;
        f.write_all(&bytes)?;
    }
    // Write the ARMED marker BEFORE the ACP process starts: its presence proves governance was
    // armed + the log is intact (evidence-integrity, same invariant as the wrapped-CLI path).
    crate::gate_hook::write_armed_marker(&decisions_path, &phase)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    Ok(AcpGovArmed {
        settings_path,
        decisions_path,
        db_path: gov.db_path.clone(),
        scope,
        phase,
        phase_id: input.unit.phase_id().unwrap_or_default().to_string(),
    })
}

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
/// - the CLI has no ACP config in the registry
/// - the ACP binary is not on PATH
/// - the handshake fails or the session dies mid-run
///
/// All fallbacks prepend a `[wicked-core] ACP …` warning to `StepOutput.output` so
/// the degradation is visible in both streaming output and persisted logs.
/// Stable `fallback_kind` slugs carried on [`CoreEvent::AcpFallback`] for UI dispatch.
pub(crate) mod fallback_kind {
    pub const BINARY_UNAVAILABLE: &str = "binary_unavailable";
    pub const HANDSHAKE_FAILED: &str = "handshake_failed";
    pub const SESSION_DIED: &str = "session_died";
    pub const HTTP_UNIMPLEMENTED: &str = "http_unimplemented";
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
    fallback: WrappedCliStepRunner,
    timeout: Duration,
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

    /// Warm (or return the existing) ACP session for one chat seat. Unlike the run
    /// path, a failed start is NOT cached as poisoned — chats are interactive, so
    /// every ensure retries and the operator sees each failure.
    fn chat_ensure(
        &self,
        chat_id: &str,
        cli_key: &str,
        cwd: &std::path::Path,
    ) -> Result<Arc<Mutex<AcpProcess>>, String> {
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
        let proc = start_acp_process(&config, cwd, None).map_err(|e| e.to_string())?;
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
        clis.iter()
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
            .collect()
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
    pub fn chat_close(&self, chat_id: &str) {
        let prefix = Self::chat_pool_key(chat_id);
        {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.retain(|(rid, _), _| rid != &prefix);
        }
        self.emit_event(CoreEvent::ChatClosed {
            chat: chat_id.to_string(),
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

        // GOVERNED ACP PATH: arm input governance and open a FRESH session per unit (not cached).
        //
        // A fresh session is required because the gate-hook subprocess inherits env vars from the
        // ACP process, and `WICKED_GATE_SCOPE`/`WICKED_GATE_PHASE` are fixed at process-start
        // time. Each unit has its own phase; reusing a cached session would leave every subsequent
        // unit governed under the FIRST unit's phase — a silent mis-routing of evidence.
        //
        // The `--settings` injection happens at `initialize` time (the binary's argv), which is
        // the only point in the ACP lifecycle where a new flag can be introduced. Per-prompt
        // injection is not possible — the `session/prompt` RPC has no settings field.
        // Input governance at ACP process start (`--settings` PreToolUse hooks) is
        // Claude-specific. Non-claude CLIs mirror the wrapped path — no input arming,
        // output-side gates still apply — and fall through to the shared ACP session
        // path below so they KEEP multi-turn ACP instead of silently degrading to
        // single-shot on every governed run.
        if let Some(gov) = input
            .governance
            .as_ref()
            .filter(|_| cli_runs_claude(&cli_key))
        {
            let acp_config = match acp_config_for(&cli_key) {
                // Only stdio-mode ACP can receive --settings at process-start time; we spawn and
                // control the process directly. HTTP-mode ACP connects to a server we don't
                // spawn, so --settings cannot be injected → fall back to the wrapped-CLI path,
                // which handles governance independently via arm_input_governance.
                Some(c) if c.transport == AcpTransport::Stdio => c,
                _ => return self.fallback.run_unit_streaming(input, emit),
            };

            let gov_armed = match arm_acp_governance(input, gov) {
                Ok(g) => {
                    // (EVT-016) GovernanceContextArmed — ACP path successfully armed governance.
                    // Fires before the ACP process starts so the operator can confirm governance
                    // is ON for this unit (distinct from GateEvaluated's after-the-fact signals).
                    self.emit_event(CoreEvent::GovernanceContextArmed {
                        session: input.run_id.clone(),
                        ord: input.unit.ord,
                        attempt: input.attempt,
                        path: "acp".to_string(),
                        db_path: g.db_path.clone(),
                    });
                    g
                }
                // A governed unit whose governance cannot be armed MUST NOT run ungoverned — fail
                // it outright, mirroring the wrapped-CLI path's fail-closed contract.
                Err(e) => {
                    return StepOutput {
                        run_id: input.run_id.clone(),
                        unit_ix: input.unit_ix,
                        attempt: input.attempt,
                        output: format!("(could not arm ACP input governance: {e})"),
                        status: StepStatus::Failed,
                        usage: None,
                        files: Vec::new(),
                        governed: false, // arming failed → not governed (unit fails anyway)
                    };
                }
            };

            let cwd = input
                .workdir
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

            // Session startup failure after governance is armed: fall back to the wrapped-CLI
            // path. WrappedCliStepRunner sees governance: Some and re-arms independently via
            // arm_input_governance — the unit is still governed, not silently unguarded.
            let mut proc = match start_acp_process(&acp_config, &cwd, Some(&gov_armed)) {
                Ok(p) => p,
                Err(e) => {
                    let reason = format!(
                        "[wicked-core] ACP governance session unavailable for '{cli_key}' \
                         ({e}); using single-shot fallback"
                    );
                    self.emit_event(CoreEvent::AcpFallback {
                        session: run_id.clone(),
                        cli_key: cli_key.clone(),
                        reason: reason.clone(),
                        fallback_kind: fallback_kind::HANDSHAKE_FAILED.to_string(),
                    });
                    return fallback_with_warning(reason, input, emit, &self.fallback);
                }
            };

            let prompt = skill_prompt(&input.unit);
            return match exec_turn_acp(&mut proc, &prompt, prior_outputs, emit, self.timeout) {
                Ok(result) if result.status == StepStatus::Ok => StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: result.output,
                    status: StepStatus::Ok,
                    usage: result.usage,
                    files: result.files,
                    governed: true,
                },
                Ok(result) if result.status == StepStatus::Cancelled => StepOutput {
                    run_id: input.run_id.clone(),
                    unit_ix: input.unit_ix,
                    attempt: input.attempt,
                    output: result.output,
                    status: StepStatus::Cancelled,
                    usage: result.usage,
                    files: result.files,
                    governed: true,
                },
                // Turn failed or session exited: fall back to wrapped-CLI, which re-arms
                // governance on its own — unit is still governed via the fallback path.
                Ok(_) => {
                    drop(proc);
                    let reason = format!(
                        "[wicked-core] ACP governance session exited for '{cli_key}'; \
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
                    let reason = format!(
                        "[wicked-core] ACP governance error for '{cli_key}' ({e}); \
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
            };
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
                match start_acp_process(&acp_config, &cwd, None) {
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
        let prompt = skill_prompt(&input.unit);

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
        r.chat_close("c1");
        assert_eq!(r.sessions.lock().unwrap().len(), 0);
        r.chat_close("c1"); // idempotent
                            // Both closes emitted ChatClosed through the actor emit point.
        let evs: Vec<_> = rx.try_iter().collect();
        let closed = evs
            .iter()
            .filter(
                |c| matches!(c, Command::EmitEvent(CoreEvent::ChatClosed { chat }) if chat == "c1"),
            )
            .count();
        assert_eq!(closed, 2);
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
}
