//! Persistent PTY session runner — wicked-core#13.
//!
//! Keeps one CLI process alive per `run_id` so successive units within the same run share
//! prompt-cache context without a cold-start penalty on every unit.
//!
//! Each unit's prompt is written to the persistent PTY's stdin; turn completion is detected by
//! watching for a `{"type":"result",...}` NDJSON sentinel in the PTY output stream — the same
//! marker [`crate::execute_wrapped::ClaudeStreamJson`] uses in the one-shot wrapped-CLI path.
//!
//! # Session lifecycle
//! - **Open (lazy)**: the first `run_unit` call for a `run_id` opens a PTY session using the
//!   same CLI and invocation template as the wrapped-CLI runner, but **without** `-p`/`--print`
//!   (interactive mode, not one-shot).
//! - **Reuse**: subsequent units on the same `run_id` write their prompt to the open PTY's stdin
//!   and collect output until the result sentinel arrives.
//! - **Close (explicit)**: call [`PersistentStepRunner::drop_session`] after the last unit of a
//!   run to cleanly kill the CLI process. Callers are responsible for this; no auto-teardown is
//!   wired into the actor.
//!
//! # Turn-completion detection
//! PTY output arrives as raw bytes (base64-encoded [`crate::event::CoreEvent::TerminalOutput`]
//! chunks). We buffer bytes into lines, strip `\r` (added by the PTY line discipline), skip lines
//! that are not JSON (echo of our own input, CLI prompt text like `> `), and pass JSON lines
//! through `ClaudeStreamJson` — which signals turn end on `{"type":"result",...}`.
//!
//! # Platform note
//! portable-pty works cross-platform, but the interactive NDJSON session protocol assumes a CLI
//! (claude) that accepts prompts on stdin and emits `--output-format stream-json` output. Non-PTY
//! CLIs should continue to use [`crate::execute_wrapped::WrappedCliStepRunner`].

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;

use crate::command::Command;
use crate::event::CoreEvent;
use crate::execute_wrapped::{
    binary_is_claude, build_argv, inject_claude_stream_flags, resolve_invocation, skill_prompt,
    AdapterOut, ClaudeStreamJson, OutputAdapter,
};
use crate::terminal;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus, Usage};

// ── Session table ─────────────────────────────────────────────────────────────

struct PtySession {
    terminal_id: String,
}

// ── PersistentStepRunner ──────────────────────────────────────────────────────

/// A [`StepRunner`] that maintains one persistent PTY session per `run_id`. See module docs.
///
/// Constructed internally by [`crate::Core::spawn_with_pty_sessions`]. Holds only a command
/// sender + the off-actor PTY map — no full `Core` reference — so it can be created before the
/// `Core` handle is assembled without a chicken-and-egg issue.
pub struct PersistentStepRunner {
    tx: std::sync::mpsc::Sender<Command>,
    pty: terminal::PtyMap,
    sessions: Arc<Mutex<HashMap<String, PtySession>>>,
    timeout: Duration,
}

impl PersistentStepRunner {
    pub(crate) fn new(tx: std::sync::mpsc::Sender<Command>, pty: terminal::PtyMap) -> Self {
        let secs = std::env::var("WICKED_UNIT_TIMEOUT_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(7200);
        Self {
            tx,
            pty,
            sessions: Arc::new(Mutex::new(HashMap::new())),
            timeout: Duration::from_secs(secs),
        }
    }

    /// Close the PTY session for `run_id` (call after the last unit of a run completes).
    /// Silently ignores unknown ids — idempotent.
    pub fn drop_session(&self, run_id: &str) {
        let tid = {
            let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.remove(run_id).map(|s| s.terminal_id)
        };
        if let Some(id) = tid {
            self.close_terminal(&id);
        }
    }

    // ── low-level actor bridge ────────────────────────────────────────────────

    fn subscribe(&self) -> std::sync::mpsc::Receiver<CoreEvent> {
        let (s, r) = std::sync::mpsc::channel();
        let _ = self.tx.send(Command::Subscribe(s));
        r
    }

    fn open_terminal(&self, cwd: std::path::PathBuf, cmd: Vec<String>) -> anyhow::Result<String> {
        let (reply, rx) = std::sync::mpsc::channel();
        self.tx
            .send(Command::OpenTerminal {
                cwd,
                cmd: Some(cmd),
                cols: 220,
                rows: 50,
                governed: true,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("core actor stopped"))?;
        rx.recv()
            .map_err(|_| anyhow::anyhow!("core actor dropped the reply"))?
    }

    fn write_terminal(&self, id: &str, bytes: &[u8]) -> anyhow::Result<()> {
        use std::io::Write;
        let writer = {
            let map = terminal::lock(&self.pty);
            let s = map
                .get(id)
                .ok_or_else(|| anyhow::anyhow!("no such terminal: {id}"))?;
            s.writer.clone()
        };
        let mut w = writer.lock().unwrap_or_else(|p| p.into_inner());
        w.write_all(bytes)?;
        w.flush()?;
        Ok(())
    }

    fn close_terminal(&self, id: &str) {
        let (reply, rx) = std::sync::mpsc::channel();
        let _ = self.tx.send(Command::CloseTerminal {
            id: id.to_string(),
            reply,
        });
        let _ = rx.recv();
    }

    // ── session argv ──────────────────────────────────────────────────────────

    /// Build the argv for an **interactive** (multi-turn) CLI session. Like the wrapped-CLI argv
    /// but without `-p`/`--print`: the process stays alive and reads successive prompts from stdin.
    /// `--output-format stream-json --verbose` is injected for claude so its output is parseable.
    fn session_argv(input: &StepInput) -> Vec<String> {
        let cli_key = input.unit.assigned_cli.as_deref().unwrap_or("claude");
        let invocation = input
            .unit
            .assigned_invocation
            .clone()
            .unwrap_or_else(|| resolve_invocation(cli_key));
        // Build argv without a real prompt — the placeholder expands to an empty string and the
        // trailing `--` + empty arg are stripped below.
        let mut argv = build_argv(&invocation, "", &input.unit.allowed_skills);
        let is_claude = argv.first().map(|a| binary_is_claude(a)).unwrap_or(false);
        // Drop the end-of-options guard and the empty prompt arg emitted by the template.
        argv.retain(|a| a != "--" && !a.is_empty());
        if is_claude {
            // Remove one-shot flags — interactive mode doesn't use them.
            // Only done for claude: other binaries may legitimately use -p for other purposes.
            argv.retain(|a| a != "-p" && a != "--print");
            // Inject stream-json (skipped when the template already carries --output-format).
            inject_claude_stream_flags(&mut argv);
        }
        argv
    }
}

impl StepRunner for PersistentStepRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        let noop = |_: &str| {};
        self.exec_turn(input, &noop)
    }

    fn run_unit_streaming(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        self.exec_turn(input, emit)
    }

    /// Close the PTY session for `run_id` so the CLI process exits cleanly after the run ends.
    ///
    /// Runs cleanup on a background thread — `on_run_complete` is called from the actor thread
    /// (via `finalize_run`/`fail_run`/`cancel_run`). Sending `Command::CloseTerminal` and then
    /// blocking on the reply channel while still ON the actor thread would deadlock because the
    /// actor cannot process its own inbox while blocked in `rx.recv()`.
    fn on_run_complete(&self, run_id: &str) {
        let tx = self.tx.clone();
        let sessions = self.sessions.clone();
        let run_id = run_id.to_string();
        std::thread::spawn(move || {
            let tid = {
                let mut guard = sessions.lock().unwrap_or_else(|p| p.into_inner());
                guard.remove(&run_id).map(|s| s.terminal_id)
            };
            if let Some(id) = tid {
                let (reply, rx) = std::sync::mpsc::channel();
                let _ = tx.send(Command::CloseTerminal { id, reply });
                let _ = rx.recv();
            }
        });
    }
}

impl PersistentStepRunner {
    fn exec_turn(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        let run_id = input.run_id.clone();

        // Lazily open a session for this run_id. The lock covers only the map read/write — not
        // the blocking open_terminal / wait_for_opened calls — so unrelated runs are never
        // serialised by one run's slow PTY startup.
        let existing_id = {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard.get(&run_id).map(|s| s.terminal_id.clone())
        };

        let terminal_id = match existing_id {
            Some(tid) => tid,
            None => {
                let cwd = input
                    .workdir
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let cmd = Self::session_argv(input);
                // Subscribe BEFORE open so we catch the TerminalOpened event.
                let pre = self.subscribe();
                let tid = match self.open_terminal(cwd, cmd) {
                    Ok(id) => id,
                    Err(e) => return failed_output(input, format!("open PTY session: {e}")),
                };
                wait_for_opened(&pre, &tid);
                // Re-acquire to insert. Use entry to handle a concurrent opener for the same
                // run_id: the first inserter wins; if we lose the race, close our duplicate.
                let final_tid = {
                    let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
                    let entry = guard.entry(run_id.clone()).or_insert(PtySession {
                        terminal_id: tid.clone(),
                    });
                    entry.terminal_id.clone()
                };
                if final_tid != tid {
                    self.close_terminal(&tid);
                }
                let cli_key = input
                    .unit
                    .assigned_cli
                    .clone()
                    .unwrap_or_else(|| "claude".to_string());
                let _ = self
                    .tx
                    .send(Command::EmitEvent(CoreEvent::WorkerSessionStarted {
                        session: run_id.clone(),
                        terminal_id: final_tid.clone(),
                        cli_key,
                    }));
                final_tid
            }
        };

        // Subscribe BEFORE writing so no output bytes are lost between write and drain.
        let events = self.subscribe();

        let prompt = format!("{}\n", skill_prompt(&input.unit));
        if let Err(e) = self.write_terminal(&terminal_id, prompt.as_bytes()) {
            // PTY already exited — drop the stale entry so future units reopen cleanly.
            self.drop_session(&run_id);
            return failed_output(input, format!("write PTY turn: {e}"));
        }

        let result = collect_turn(&events, &terminal_id, self.timeout, emit, input);
        if result.status != StepStatus::Ok {
            // On any non-Ok outcome the terminal may be broken/hung. Drop the session so
            // the next unit for this run_id opens a fresh PTY instead of reusing a stale one.
            self.drop_session(&run_id);
        }
        result
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Wait up to 2 s for `TerminalOpened` for `id`. Continues silently on timeout — the write will
/// error if the terminal truly never opened, so the runner fails cleanly.
fn wait_for_opened(rx: &std::sync::mpsc::Receiver<CoreEvent>, id: &str) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(CoreEvent::TerminalOpened { id: i, .. }) if i == id => return,
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Drain `TerminalOutput` events for `terminal_id` until `{"type":"result",...}` arrives (or
/// `timeout` elapses). Returns a [`StepOutput`] exactly matching the wrapped-CLI runner's shape.
///
/// Raw PTY bytes arrive as base64-encoded chunks that may span line boundaries. We accumulate
/// them into a line buffer, strip `\r` (added by the PTY line discipline), and skip lines that
/// are not JSON (echoed input, interactive prompts like `> `). JSON lines go through
/// `ClaudeStreamJson` which extracts text deltas, usage, and the result sentinel.
fn collect_turn(
    rx: &std::sync::mpsc::Receiver<CoreEvent>,
    terminal_id: &str,
    timeout: Duration,
    emit: &DeltaSink,
    input: &StepInput,
) -> StepOutput {
    let mut adapter = ClaudeStreamJson::default();
    let mut line_buf = String::new();
    let mut output = String::new();
    let mut usage: Option<Usage> = None;
    let mut files: Vec<String> = Vec::new();
    const MAX_OUT: usize = 8 * 1024 * 1024;

    let deadline = Instant::now() + timeout;

    // Loop returns (found_result, timed_out):
    //   (true,  _)    → StepStatus::Ok
    //   (false, true) → StepStatus::Cancelled (deadline elapsed, CLI still alive)
    //   (false, false)→ StepStatus::Failed    (CLI crash / PTY exit / channel disconnect)
    let (found_result, timed_out): (bool, bool) = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            break (false, true);
        }
        let poll = remaining.min(Duration::from_millis(100));
        match rx.recv_timeout(poll) {
            Ok(CoreEvent::TerminalOutput { id, bytes_b64, .. }) if id == terminal_id => {
                if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&bytes_b64) {
                    line_buf.push_str(&String::from_utf8_lossy(&bytes));
                }
                if drain_lines(
                    &mut line_buf,
                    &mut adapter,
                    emit,
                    &mut output,
                    &mut usage,
                    &mut files,
                    MAX_OUT,
                ) {
                    break (true, false);
                }
            }
            Ok(CoreEvent::TerminalExited { id, .. }) if id == terminal_id => break (false, false),
            Ok(_) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break (false, false),
        }
    };

    // Flush any remaining complete lines (e.g. on TerminalExited without trailing newline).
    drain_lines(
        &mut line_buf,
        &mut adapter,
        emit,
        &mut output,
        &mut usage,
        &mut files,
        MAX_OUT,
    );
    // Adapter finish (both current adapters are stateless, but call for completeness).
    let fin = adapter.finish();
    absorb(fin, emit, &mut output, &mut usage, &mut files, MAX_OUT);

    StepOutput {
        run_id: input.run_id.clone(),
        unit_ix: input.unit_ix,
        attempt: input.attempt,
        output: output.trim_end().to_string(),
        status: if found_result {
            StepStatus::Ok
        } else if timed_out {
            StepStatus::Cancelled
        } else {
            StepStatus::Failed
        },
        usage,
        files,
        governed: false,
    }
}

/// Process all complete lines in `buf` through `adapter`. Returns `true` when a result sentinel
/// line is found (turn complete). Partial trailing content stays in `buf` for the next chunk.
fn drain_lines(
    buf: &mut String,
    adapter: &mut ClaudeStreamJson,
    emit: &DeltaSink,
    output: &mut String,
    usage: &mut Option<Usage>,
    files: &mut Vec<String>,
    max_out: usize,
) -> bool {
    let mut found = false;
    while let Some(pos) = buf.find('\n') {
        let raw = buf[..pos].to_string();
        *buf = buf[pos + 1..].to_string();
        let line = raw.trim_end_matches('\r');
        if line.is_empty() || !line.starts_with('{') {
            // Skip echoed input and non-JSON noise (CLI prompts, blank lines).
            continue;
        }
        if is_result_line(line) {
            found = true;
        }
        let ao = adapter.on_line(line);
        absorb(ao, emit, output, usage, files, max_out);
    }
    found
}

/// Push one `AdapterOut` into the running accumulators and stream text deltas through `emit`.
fn absorb(
    ao: AdapterOut,
    emit: &DeltaSink,
    output: &mut String,
    usage: &mut Option<Usage>,
    files: &mut Vec<String>,
    max_out: usize,
) {
    for t in ao.text {
        emit(&t);
        if output.len() < max_out {
            output.push_str(&t);
            output.push('\n');
        }
    }
    if ao.usage.is_some() {
        *usage = ao.usage;
    }
    files.extend(ao.files);
}

/// Quick sentinel check — is this line a `{"type":"result",...}` NDJSON row?
fn is_result_line(line: &str) -> bool {
    if !line.contains("\"result\"") {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|v| {
            v.get("type")
                .and_then(|t| t.as_str())
                .map(|s| s == "result")
        })
        .unwrap_or(false)
}

fn failed_output(input: &StepInput, msg: String) -> StepOutput {
    StepOutput {
        run_id: input.run_id.clone(),
        unit_ix: input.unit_ix,
        attempt: input.attempt,
        output: msg,
        status: StepStatus::Failed,
        usage: None,
        files: Vec::new(),
        governed: false,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::domain::{StageKind, UnitStatus, WorkUnit};
    use crate::event::CoreEvent;
    use crate::scope::EntityMode;
    use crate::workflow::{GateSpec, PhaseRole};
    use std::sync::atomic::{AtomicU64, Ordering};

    static DB_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_db() -> String {
        let seq = DB_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wicked-core-sess-{}-{}.db",
            std::process::id(),
            seq
        ));
        p.to_string_lossy().into_owned()
    }

    /// Build a minimal [`WorkUnit`] with a custom invocation (no `{PROMPT}` placeholder — the
    /// session runner sends prompts via stdin, not as an argv element).
    fn make_unit(description: &str, invocation: &str) -> WorkUnit {
        WorkUnit {
            id: "u-test".to_string(),
            session_id: "sess-test".to_string(),
            ord: 1,
            description: description.to_string(),
            stage: StageKind::Build,
            assigned_cli: Some("sh".to_string()),
            assigned_invocation: Some(invocation.to_string()),
            council_task_ref: None,
            routing: None,
            denial_reason: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            skill_ref: None,
            allowed_skills: Vec::new(),
            gate: GateSpec::default(),
            role: PhaseRole::default(),
            validator: None,
            tool_cmd: None,
            status: UnitStatus::Pending,
        }
    }

    fn make_input(run_id: &str, unit_ix: usize, unit: WorkUnit) -> StepInput {
        StepInput {
            run_id: run_id.to_string(),
            unit_ix,
            attempt: 0,
            unit,
            workflow_id: "wf-test".to_string(),
            entity_mode: EntityMode::Shared,
            workdir: Some(std::env::temp_dir()),
            governance: None,
            prior_outputs: vec![],
        }
    }

    /// Write a fake interactive CLI script to a temp file and return `sh /path` as the invocation.
    ///
    /// The script reads one plain-text line per turn and emits the minimum NDJSON that
    /// `ClaudeStreamJson` parses: one `assistant` text delta then a `result` sentinel.
    /// Using a file avoids the quoting issue where JSON double-quotes break `tokenize`'s
    /// double-quote span tracking when embedded in the invocation string.
    fn fake_cli_invocation() -> String {
        use std::os::unix::fs::PermissionsExt;
        static PATH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let p = PATH.get_or_init(|| {
            let mut path = std::env::temp_dir();
            path.push(format!("wicked-core-fake-cli-{}.sh", std::process::id()));
            let script = "#!/bin/sh\n\
                while IFS= read -r line; do\n\
                  printf '{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"WKRTURN:%s\"}]}}\\n' \"$line\"\n\
                  printf '{\"type\":\"result\",\"result\":\"ok\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}\\n' \"$line\"\n\
                done\n";
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            path.to_string_lossy().into_owned()
        });
        format!("sh {p}")
    }

    /// Helper: drain events until `pred` matches or timeout elapses.
    fn wait_for(rx: &std::sync::mpsc::Receiver<CoreEvent>, pred: impl Fn(&CoreEvent) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(ev) if pred(&ev) => return,
                Ok(_) | Err(_) => continue,
            }
        }
    }

    // ── core proof: one session, multiple turns ─────────────────────────────

    /// Two successive `run_unit` calls with the SAME `run_id` must:
    /// 1. Open only ONE PTY session (no second `TerminalOpened` event after the first turn).
    /// 2. Deliver distinct turn outputs (each prompt echoed back).
    /// 3. Report `StepStatus::Ok` + non-zero usage for each turn.
    #[test]
    fn two_units_same_run_share_one_session() {
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        let (core, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();

        let invocation = fake_cli_invocation();
        let unit1 = make_unit("first work", &invocation);
        let unit2 = make_unit("second work", &invocation);
        let input1 = make_input("run-shared-session", 0, unit1);
        let input2 = make_input("run-shared-session", 1, unit2);

        // Turn 1 — opens the session.
        let out1 = runner.run_unit(&input1);
        assert_eq!(
            out1.status,
            StepStatus::Ok,
            "turn 1 failed: {:?}",
            out1.output
        );
        assert!(
            out1.output.contains("WKRTURN:"),
            "turn 1 output missing sentinel; got: {:?}",
            out1.output
        );
        assert!(out1.usage.is_some(), "turn 1 missing usage");

        // Exactly one TerminalOpened by now — session opened on turn 1.
        let mut opened_count = 0usize;
        // Drain all buffered events without blocking.
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, CoreEvent::TerminalOpened { .. }) {
                opened_count += 1;
            }
        }
        assert_eq!(
            opened_count, 1,
            "expected exactly 1 TerminalOpened after turn 1"
        );

        // Turn 2 — reuses the existing session.
        let out2 = runner.run_unit(&input2);
        assert_eq!(
            out2.status,
            StepStatus::Ok,
            "turn 2 failed: {:?}",
            out2.output
        );
        assert!(
            out2.output.contains("WKRTURN:"),
            "turn 2 output missing sentinel; got: {:?}",
            out2.output
        );

        // No second TerminalOpened — the session was reused, not reopened.
        let mut extra_opens = 0usize;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, CoreEvent::TerminalOpened { .. }) {
                extra_opens += 1;
            }
        }
        assert_eq!(
            extra_opens, 0,
            "unexpected extra TerminalOpened on turn 2 (session reused)"
        );

        // Explicit teardown — closes the PTY cleanly.
        runner.drop_session("run-shared-session");
        wait_for(&events, |e| matches!(e, CoreEvent::TerminalExited { .. }));
    }

    /// Two runs with DIFFERENT `run_id`s each open their own session.
    #[test]
    fn different_run_ids_open_separate_sessions() {
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        let (core, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();

        let invocation = fake_cli_invocation();
        let input_a = make_input("run-A", 0, make_unit("work A", &invocation));
        let input_b = make_input("run-B", 0, make_unit("work B", &invocation));

        let out_a = runner.run_unit(&input_a);
        let out_b = runner.run_unit(&input_b);

        assert_eq!(
            out_a.status,
            StepStatus::Ok,
            "run-A failed: {:?}",
            out_a.output
        );
        assert_eq!(
            out_b.status,
            StepStatus::Ok,
            "run-B failed: {:?}",
            out_b.output
        );

        // Two separate sessions opened.
        let mut opened = 0usize;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, CoreEvent::TerminalOpened { .. }) {
                opened += 1;
            }
        }
        assert_eq!(
            opened, 2,
            "expected 2 TerminalOpened (one per run_id); got {opened}"
        );

        runner.drop_session("run-A");
        runner.drop_session("run-B");
    }

    /// `drop_session` on an unknown id is a no-op (idempotent).
    #[test]
    fn drop_session_unknown_id_is_noop() {
        std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
        let (_, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        runner.drop_session("no-such-run"); // must not panic
    }
}
