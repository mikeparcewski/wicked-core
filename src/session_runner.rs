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
    binary_is_claude, build_argv, inject_claude_stream_flags, pty_unit_prompt, resolve_invocation,
    skills_refusal, AdapterOut, ClaudeStreamJson, OutputAdapter, SkillForm,
};
use crate::terminal;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus, Usage};

// ── Session table ─────────────────────────────────────────────────────────────

struct PtySession {
    terminal_id: String,
    /// Whether the session was opened for a NO-CODE phase (read-only posture, F-036). A session
    /// serves only turns of its own posture: a creator's write-posture session is never reused by
    /// an `executes_code: false` phase — it could background a write past the guard's final
    /// snapshot — and a read-only session is never handed to a code phase that must write.
    no_code: bool,
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

/// The carrier's name in a skills refusal (`SkillsError::CarrierWithoutSkills`).
const PTY_CARRIER: &str = "persistent PTY";

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

    /// The invocation template a unit's session runs: the unit's own (an ad-hoc launch CLI not in
    /// the registry), else the registry's for its CLI key. ONE resolution for both the session
    /// argv and the prompt's skill form (core#396), so the two cannot name different binaries.
    fn session_invocation(input: &StepInput) -> String {
        let cli_key = input.unit.assigned_cli.as_deref().unwrap_or("claude");
        input
            .unit
            .assigned_invocation
            .clone()
            .unwrap_or_else(|| resolve_invocation(cli_key))
    }

    /// Build the argv for an **interactive** (multi-turn) CLI session from the ONE resolved
    /// `invocation` template (`session_invocation`, resolved once per turn by `exec_turn` and shared
    /// with the prompt's skill form — Copilot, review pass 7: a second resolution could see a
    /// reloaded registry and name another binary). Like the wrapped-CLI argv but without
    /// `-p`/`--print`: the process stays alive and reads successive prompts from stdin.
    /// `--output-format stream-json --verbose` is injected for claude so its output is parseable.
    /// `Err` is a refused launch (F-036): a NO-CODE unit on a lever-less non-claude seat whose
    /// template grants writes — the same boundary the wrapped runner applies
    /// (`execute_wrapped::apply_no_code_posture`), so no carrier can launch an evaluator with a
    /// write-capable posture.
    fn session_argv(invocation: &str, input: &StepInput) -> Result<Vec<String>, String> {
        // Build argv without a real prompt — the placeholder expands to an empty string and the
        // trailing `--` + empty arg are stripped below.
        let mut argv = build_argv(invocation, "", &input.unit.allowed_skills);
        let is_claude = argv.first().map(|a| binary_is_claude(a)).unwrap_or(false);
        // Drop the end-of-options guard and the empty prompt arg emitted by the template.
        argv.retain(|a| a != "--" && !a.is_empty());
        if is_claude {
            // Remove one-shot flags — interactive mode doesn't use them.
            // Only done for claude: other binaries may legitimately use -p for other purposes.
            argv.retain(|a| a != "-p" && a != "--print");
            // Inject stream-json (skipped when the template already carries --output-format).
            inject_claude_stream_flags(&mut argv);
        } else if crate::write_posture::WritePosture::of(&input.unit, input.workdir.is_some())
            == crate::write_posture::WritePosture::ReadOnly
        {
            // F-036: a READ-ONLY phase (an `executes_code: false` evaluator/recon rung — never a
            // creator, F-4R2-004) on a non-claude seat crosses the shared launch boundary —
            // the template's tokens are rewritten to the seat's read-only lever (codex
            // `--sandbox read-only`, pi `--exclude-tools edit,write`), a write grant on a
            // lever-less seat refuses the launch. This carrier resolves no `trust_flags`, so the
            // template is the whole posture here.
            // Recognition is by the RESOLVED binary's stem (argv[0]), never the seat's key.
            let lever = crate::execute_wrapped::apply_no_code_posture(&mut argv, Vec::new())?;
            eprintln!(
                "wicked-core: unit {} (phase `{}`, executes_code:false) opens a persistent \
                 session on '{}' with the read-only posture {} (F-036)",
                input.unit.ord,
                input.unit.phase_id().unwrap_or("?"),
                argv.first().map(String::as_str).unwrap_or("?"),
                lever.describe()
            );
        }
        Ok(argv)
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
                // (EVT-004) Normal end-of-run session close. Emit before CloseTerminal so
                // observers see the closed event in the stream before the PTY child exits.
                let _ = tx.send(Command::EmitEvent(CoreEvent::WorkerSessionClosed {
                    session: run_id.clone(),
                    terminal_id: id.clone(),
                    reason: "run_complete".to_string(),
                }));
                let (reply, rx) = std::sync::mpsc::channel();
                let _ = tx.send(Command::CloseTerminal { id, reply });
                let _ = rx.recv();
            }
        });
    }

    /// Purge the per-run session cache entry on reassignment so a subsequent dispatch to the same
    /// `run_id` opens a fresh PTY instead of reusing the now-closed one.
    ///
    /// The terminal is already closed by the actor via `finish_terminal` before this is called,
    /// so we only remove the stale cache entry — no `CloseTerminal` command is sent here.
    fn close_cli_session(&self, run_id: &str, _cli_key: &str) {
        let mut guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        guard.remove(run_id);
    }
}

impl PersistentStepRunner {
    fn exec_turn(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        let run_id = input.run_id.clone();
        // core#396 (codex round 8, ADJUDICATED; round 9): this carrier opens the raw CLI — no
        // snapshot resolution, no admission, no isolation flags, no delivery lever — so it cannot
        // hand a skill to the worker. A run with ANY skill-bearing unit is REFUSED here at its
        // FIRST unit, before any session is opened or written to: the actor hands every unit the
        // run's whole skill set (`StepInput::required_skills`, read off the plan), so a skill-free
        // first unit does no work ahead of a later unit this carrier could never serve. The current
        // unit's own `skill_ref` is unioned in (a unit dispatched outside the actor's plan carries
        // no plan set). No invocation directive is ever emitted on this carrier; a run that names
        // no skill anywhere runs exactly as before.
        let mut skills: Vec<String> = input
            .required_skills
            .iter()
            .filter(|s| !s.is_empty())
            .cloned()
            .chain(
                input
                    .unit
                    .skill_ref
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(str::to_string),
            )
            .collect();
        skills.sort();
        skills.dedup();
        if !skills.is_empty() {
            return skills_refusal(
                input,
                &crate::skills_snapshot::SkillsError::CarrierWithoutSkills {
                    carrier: PTY_CARRIER.to_string(),
                    skills,
                },
            );
        }
        // ONE resolution of the invocation template for this turn: the session argv (when a
        // session is opened) and the prompt's skill form both read THIS value, so they cannot name
        // different binaries even if the registry is reloaded between the two uses.
        let invocation = Self::session_invocation(input);

        // Lazily open a session for this run_id. The lock covers only the map read/write — not
        // the blocking open_terminal / wait_for_opened calls — so unrelated runs are never
        // serialised by one run's slow PTY startup.
        // F-036 (codex review on #414): a session is reused ONLY when its posture matches this
        // phase's. A no-code phase reaching a creator-opened (write-posture) session closes it and
        // opens a fresh read-only one; a code phase reaching a read-only session likewise.
        // F-4R2-004: derived from the unit's ROLE and the run's tree — a fenced unit (read-only or
        // deliverable-roots) never shares a session with a write-posture one.
        let wants_no_code =
            crate::write_posture::WritePosture::of(&input.unit, input.workdir.is_some())
                .fences_writes();
        let existing_id = {
            let guard = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
            guard
                .get(&run_id)
                .map(|s| (s.terminal_id.clone(), s.no_code))
        };
        let existing_id = match existing_id {
            Some((tid, no_code)) if no_code == wants_no_code => Some(tid),
            Some((tid, _)) => {
                eprintln!(
                    "wicked-core: run {run_id} unit {} needs a {} session but the open PTY session \
                     {tid} was opened {} — closing it and opening a fresh one (F-036)",
                    input.unit.ord,
                    if wants_no_code { "fenced" } else { "write-posture" },
                    if wants_no_code { "with write posture" } else { "fenced" }
                );
                self.drop_session(&run_id);
                None
            }
            None => None,
        };

        let terminal_id = match existing_id {
            Some(tid) => {
                // (EVT-003) Session already open — reusing it for this unit. Fires before the
                // prompt write so callers can observe the reuse before any output arrives.
                let _ = self
                    .tx
                    .send(Command::EmitEvent(CoreEvent::WorkerSessionReused {
                        session: run_id.clone(),
                        terminal_id: tid.clone(),
                        ord: input.unit.ord,
                    }));
                tid
            }
            None => {
                let cwd = input
                    .workdir
                    .clone()
                    .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
                let cmd = match Self::session_argv(&invocation, input) {
                    Ok(cmd) => cmd,
                    Err(why) => {
                        return failed_output(
                            input,
                            format!("read-only posture refused the launch: {why}"),
                        )
                    }
                };
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
                        no_code: wants_no_code,
                    });
                    entry.terminal_id.clone()
                };
                if final_tid != tid {
                    // Lost the race — another thread already inserted for this run_id.
                    // Close our duplicate and don't emit: the winner already emitted or will.
                    self.close_terminal(&tid);
                } else {
                    let cli_key = input
                        .unit
                        .assigned_cli
                        .as_deref()
                        .unwrap_or("claude")
                        .to_string();
                    let _ = self
                        .tx
                        .send(Command::EmitEvent(CoreEvent::WorkerSessionStarted {
                            session: run_id.clone(),
                            terminal_id: tid, // final_tid == tid; move instead of clone
                            cli_key,
                        }));
                }
                final_tid
            }
        };

        // Subscribe BEFORE writing so no output bytes are lost between write and drain.
        let events = self.subscribe();

        // Line-length is a correctness constraint here, not a nicety: an over-long line is dropped by
        // the terminal with no error, so the alternative to failing now is a turn that waits out its
        // full timeout for output the CLI was never given the chance to produce.
        // The directive is spelled for the binary this session runs (core#396) — the SAME resolved
        // template `session_argv` opened it with (one `session_invocation` call per turn, above).
        let form = SkillForm::for_invocation(&invocation);
        let prompt = match pty_unit_prompt(input, form) {
            Ok(p) => format!("{p}\n"),
            Err(e) => return failed_output(input, e),
        };
        if let Err(e) = self.write_terminal(&terminal_id, prompt.as_bytes()) {
            // (EVT-004) The PTY write failed — emit the closed event before dropping the session
            // so observers see the full lifecycle (opened → reused? → closed:error).
            let _ = self
                .tx
                .send(Command::EmitEvent(CoreEvent::WorkerSessionClosed {
                    session: run_id.clone(),
                    terminal_id: terminal_id.clone(),
                    reason: "error".to_string(),
                }));
            // PTY already exited — drop the stale entry so future units reopen cleanly.
            self.drop_session(&run_id);
            return failed_output(input, format!("write PTY turn: {e}"));
        }

        let result = collect_turn(
            &events,
            &terminal_id,
            self.timeout,
            emit,
            input,
            Some(&self.tx),
        );
        if result.status != StepStatus::Ok {
            // On any non-Ok outcome the terminal may be broken/hung. Drop the session so
            // the next unit for this run_id opens a fresh PTY instead of reusing a stale one.
            self.drop_session(&run_id);
        } else if wants_no_code {
            // F-036 QUIESCE (codex review on #414): a NO-CODE phase's session is closed as soon as
            // its turn is over — the PTY teardown `killpg`s the whole process group (terminal.rs),
            // so a writer the seat backgrounded cannot land after the worktree guard's FINAL
            // snapshot, which the worker thread takes when this returns. Such a session is never
            // reused anyway (the next phase either writes, or is another no-code phase that opens
            // its own).
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
    stall_tx: Option<&std::sync::mpsc::Sender<Command>>,
) -> StepOutput {
    let mut adapter = ClaudeStreamJson::default();
    let mut line_buf = String::new();
    let mut output = String::new();
    let mut usage: Option<Usage> = None;
    let mut files: Vec<String> = Vec::new();
    // Tool NAMES this session's CLI invoked (FINDING-046). Same capture as the wrapped path — this
    // persistent-session path drives the same `ClaudeStreamJson` adapter, so per-tool observability
    // must be symmetric or a governed unit that happens to run in a session would be blind.
    let mut tools: Vec<String> = Vec::new();
    const MAX_OUT: usize = 8 * 1024 * 1024;

    let deadline = Instant::now() + timeout;
    // STALL DETECTION: a live PTY that stops producing bytes mid-turn may be sitting at
    // an interactive prompt the stream parser can never answer. After WICKED_STALL_SECS
    // (default 120) of silence we emit WorkerStalled ONCE so the operator can inspect
    // the terminal or inject a response — the turn keeps waiting (the overall timeout
    // still bounds it).
    let stall_after = Duration::from_secs(
        std::env::var("WICKED_STALL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(120),
    );
    let mut last_activity = Instant::now();
    let mut stall_emitted = false;

    // Loop returns (found_result, timed_out):
    //   (true,  _)    → StepStatus::Ok
    //   (false, true) → StepStatus::TimedOut (the engine's own deadline elapsed, CLI still alive
    //                   — distinguishable from an operator cancel, which never reaches this loop)
    //   (false, false)→ StepStatus::Failed   (CLI crash / PTY exit / channel disconnect)
    let (found_result, timed_out): (bool, bool) = loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            break (false, true);
        }
        // Checked every iteration (not just on recv timeout): a busy bus — other
        // terminals/runs producing events — keeps recv returning Ok, so a timeout-arm-only
        // check could starve forever while THIS terminal is silent.
        if !stall_emitted && last_activity.elapsed() >= stall_after {
            stall_emitted = true;
            if let Some(tx) = stall_tx {
                let _ = tx.send(Command::EmitEvent(CoreEvent::WorkerStalled {
                    session: input.run_id.clone(),
                    ord: input.unit.ord,
                    terminal_id: terminal_id.to_string(),
                    stalled_secs: last_activity.elapsed().as_secs(),
                }));
            }
        }
        let poll = remaining.min(Duration::from_millis(100));
        match rx.recv_timeout(poll) {
            Ok(CoreEvent::TerminalOutput { id, bytes_b64, .. }) if id == terminal_id => {
                last_activity = Instant::now();
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
                    &mut tools,
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
        &mut tools,
        MAX_OUT,
    );
    // Adapter finish (both current adapters are stateless, but call for completeness).
    let fin = adapter.finish();
    absorb(
        fin,
        emit,
        &mut output,
        &mut usage,
        &mut files,
        &mut tools,
        MAX_OUT,
    );

    StepOutput {
        run_id: input.run_id.clone(),
        unit_ix: input.unit_ix,
        attempt: input.attempt,
        output: output.trim_end().to_string(),
        status: if found_result {
            StepStatus::Ok
        } else if timed_out {
            StepStatus::TimedOut
        } else {
            StepStatus::Failed
        },
        usage,
        files,
        tools,
        governed: false,
    }
}

/// Process all complete lines in `buf` through `adapter`. Returns `true` when a result sentinel
/// line is found (turn complete). Partial trailing content stays in `buf` for the next chunk.
#[allow(clippy::too_many_arguments)] // parallel accumulators (output/usage/files/tools) travel together
fn drain_lines(
    buf: &mut String,
    adapter: &mut ClaudeStreamJson,
    emit: &DeltaSink,
    output: &mut String,
    usage: &mut Option<Usage>,
    files: &mut Vec<String>,
    tools: &mut Vec<String>,
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
        absorb(ao, emit, output, usage, files, tools, max_out);
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
    tools: &mut Vec<String>,
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
    // Capped like the wrapped path (FINDING-046 review): a looping CLI can emit unboundedly many
    // `tool_use` blocks, so retain only up to the shared ceiling.
    crate::execute_wrapped::retain_tools_capped(tools, ao.tools);
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
        tools: Vec::new(),
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
            denial: None,
            phase_ref: None,
            conformance_ref: None,
            phase_status: None,
            collection_scope: None,
            skill_ref: None,
            allowed_skills: Vec::new(),
            gate: GateSpec::default(),
            role: PhaseRole::default(),
            validator: None,
            required_deliverables: Vec::new(),
            executes_code: false,
            tool_cmd: None,
            worker_failed_clis: Vec::new(),
            depends_on: Vec::new(),
            pre_build_scope: false,
            scope_warnings: Vec::new(),
            worktree_guarded: false,
            worktree_baseline: None,
            worktree_mutation: None,
            repo_checks_floor: false,
            default_floor: false,
            repo_checks: None,
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
            elicitation_epoch: 0,
            process_gen: None,
            launch_seq: 0,
            required_skills: Vec::new(),
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

    /// RAII pin of `WICKED_MEMORY_EMBEDDER` (codex round 9, L2): hold `test_env::ENV_LOCK` (write)
    /// first and declare the pin AFTER the lock guard, so it restores before the lock releases —
    /// the process-global mutation never leaks into a concurrently running test.
    struct EmbedderPin(Option<std::ffi::OsString>);

    impl EmbedderPin {
        fn hash() -> Self {
            let prev = std::env::var_os("WICKED_MEMORY_EMBEDDER");
            std::env::set_var("WICKED_MEMORY_EMBEDDER", "hash");
            Self(prev)
        }
    }

    impl Drop for EmbedderPin {
        fn drop(&mut self) {
            match &self.0 {
                Some(v) => std::env::set_var("WICKED_MEMORY_EMBEDDER", v),
                None => std::env::remove_var("WICKED_MEMORY_EMBEDDER"),
            }
        }
    }

    /// A fake interactive CLI like [`fake_cli_invocation`] that, per turn, APPENDS the prompt line
    /// to `marker` before answering — so a test can prove a session never received a turn (the
    /// marker is never created).
    fn fake_cli_with_marker(marker: &std::path::Path) -> String {
        use std::os::unix::fs::PermissionsExt;
        let mut path = std::env::temp_dir();
        path.push(format!(
            "wicked-core-fake-cli-marker-{}-{}.sh",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let script = format!(
            "#!/bin/sh\n\
             while IFS= read -r line; do\n\
               printf '%s\\n' \"$line\" >> '{}'\n\
               printf '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"WKRTURN:%s\"}}]}}}}\\n' \"$line\"\n\
               printf '{{\"type\":\"result\",\"result\":\"ok\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}\\n' \"$line\"\n\
             done\n",
            marker.display()
        );
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        format!("sh {}", path.to_string_lossy())
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
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
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

    /// F-036 (codex review on #414): the persistent PTY carrier crosses the SAME no-code launch
    /// boundary as the wrapped runner — recognition by the RESOLVED binary's stem, so a fake CLI
    /// NAMED `codex` gets `--sandbox read-only` appended (the template's own tokens rewritten), a
    /// code phase on the same binary is untouched, and an unknown binary whose template grants
    /// writes is refused before any PTY opens.
    #[test]
    fn a_no_code_unit_opens_its_pty_session_read_only_and_a_write_grant_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let dir = std::env::temp_dir().join(format!(
            "wicked-sess-posture-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let codex = dir.join("codex");
        std::fs::write(&codex, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let codex = codex.to_string_lossy().into_owned();

        // A NO-CODE unit on a binary named codex: the template's workspace-write is rewritten.
        let mut unit = make_unit(
            "verify the fix",
            &format!("{codex} --sandbox workspace-write"),
        );
        unit.assigned_cli = Some("reviewer".to_string()); // the key is irrelevant to recognition
        unit.worktree_guarded = true;
        let input = make_input("run-ro", 0, unit);
        let argv = PersistentStepRunner::session_argv(
            &format!("{codex} --sandbox workspace-write"),
            &input,
        )
        .expect("codex has a lever");
        assert_eq!(argv, s(&[&codex, "--sandbox", "read-only"]));
        // No sandbox in the template: the lever is appended.
        let mut unit = make_unit("verify the fix", &format!("{codex} exec"));
        unit.worktree_guarded = true;
        let input = make_input("run-ro-append", 0, unit);
        assert_eq!(
            PersistentStepRunner::session_argv(&format!("{codex} exec"), &input).unwrap(),
            s(&[&codex, "exec", "--sandbox", "read-only"])
        );
        // `--yolo` (codex's alias of the bypass, which beats a later read-only sandbox) is dropped
        // on this carrier too — adversarial review on #414.
        assert_eq!(
            PersistentStepRunner::session_argv(&format!("{codex} --yolo exec"), &input).unwrap(),
            s(&[&codex, "exec", "--sandbox", "read-only"])
        );
        // A CODE phase is untouched — the guard reads the def, never guesses.
        let unit = make_unit("build it", &format!("{codex} --sandbox workspace-write"));
        let input = make_input("run-code", 0, unit);
        assert_eq!(
            PersistentStepRunner::session_argv(
                &format!("{codex} --sandbox workspace-write"),
                &input
            )
            .unwrap(),
            s(&[&codex, "--sandbox", "workspace-write"])
        );
        // A seat NAMED codex that runs some other binary is unknown: its write-capable sandbox is
        // refused, never rewritten.
        let mut unit = make_unit(
            "verify the fix",
            "/opt/other/tool --sandbox workspace-write",
        );
        unit.assigned_cli = Some("codex".to_string());
        unit.worktree_guarded = true;
        let input = make_input("run-alias-refused", 0, unit);
        assert!(PersistentStepRunner::session_argv(
            "/opt/other/tool --sandbox workspace-write",
            &input
        )
        .is_err());
        // An unknown binary whose TEMPLATE grants writes: refused before any PTY opens.
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
        let (core, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();
        let mut unit = make_unit("verify the fix", "copilot --allow-all-tools -p");
        unit.assigned_cli = Some("copilot".to_string());
        unit.worktree_guarded = true;
        let input = make_input("run-refused", 0, unit);
        let out = runner.run_unit(&input);
        assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
        assert!(
            out.output.contains("read-only posture refused the launch")
                && out.output.contains("--allow-all-tools"),
            "{}",
            out.output
        );
        while let Ok(ev) = events.try_recv() {
            assert!(
                !matches!(ev, CoreEvent::TerminalOpened { .. }),
                "a refused launch must open no PTY"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// F-036 (codex review on #414): a creator phase opens a write-posture session; the evaluator
    /// phase that follows must NOT reuse it — it gets a FRESH session (and the fake CLI sees the
    /// read-only argv), and when its turn is over the session is CLOSED, so a writer the seat
    /// backgrounded dies with the process group before the worktree guard's final snapshot.
    #[test]
    fn an_evaluator_phase_never_reuses_a_creators_session_and_its_own_is_quiesced_after_the_turn() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
        let dir = std::env::temp_dir().join(format!(
            "wicked-sess-reopen-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let late = dir.join("late.txt");
        // A fake CLI NAMED codex: echoes its argv in every turn's text, and on every turn
        // backgrounds a delayed writer — the shape a quiesce must defeat.
        let codex = dir.join("codex");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\nARGS=\"$*\"\nwhile IFS= read -r line; do\n  (sleep 1; echo late >> \"{}\") &\n  printf '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"WKRTURN:%s|ARGV:%s\"}}]}}}}\\n' \"$line\" \"$ARGS\"\n  printf '{{\"type\":\"result\",\"result\":\"ok\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}\\n'\ndone\n",
                late.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let invocation = format!("{} --sandbox workspace-write", codex.display());

        let (core, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();

        // Turn 1 — the CREATOR (a code phase): opens a write-posture session.
        let mut creator = make_unit("build the fix", &invocation);
        creator.executes_code = true;
        let out1 = runner.run_unit(&make_input("run-reopen", 0, creator));
        assert_eq!(out1.status, StepStatus::Ok, "{}", out1.output);
        assert!(
            out1.output.contains("ARGV:--sandbox workspace-write"),
            "the creator's session keeps its write posture: {}",
            out1.output
        );
        let mut opened = 0usize;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, CoreEvent::TerminalOpened { .. }) {
                opened += 1;
            }
        }
        assert_eq!(opened, 1, "one session so far");

        // Turn 2 — the EVALUATOR (executes_code: false): must not reuse the creator's session.
        let mut evaluator = make_unit("verify the fix", &invocation);
        evaluator.worktree_guarded = true;
        let out2 = runner.run_unit(&make_input("run-reopen", 1, evaluator));
        assert_eq!(out2.status, StepStatus::Ok, "{}", out2.output);
        assert!(
            out2.output.contains("ARGV:--sandbox read-only")
                && !out2.output.contains("workspace-write"),
            "the evaluator got a FRESH read-only session: {}",
            out2.output
        );
        let (mut reopened, mut exited) = (0usize, 0usize);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match events.try_recv() {
                Ok(CoreEvent::TerminalOpened { .. }) => reopened += 1,
                Ok(CoreEvent::TerminalExited { .. }) => exited += 1,
                Ok(_) => {}
                Err(_) => {
                    if exited >= 2 {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }
        }
        assert_eq!(reopened, 1, "the evaluator opened its own session");
        assert!(
            exited >= 2,
            "the creator's session was closed on the posture change AND the evaluator's own \\
             session was closed when its turn ended (quiesce); saw {exited} exits"
        );
        // The backgrounded writers died with their process groups: nothing lands late.
        std::thread::sleep(std::time::Duration::from_millis(2500));
        assert!(
            !late.exists(),
            "a writer the seat backgrounded must die with the quiesced session"
        );
        runner.drop_session("run-reopen");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Adversarial review on #414: the PTY teardown SIGKILLs the process group UNCONDITIONALLY
    /// after the TERM grace. A descendant that traps TERM and detaches its stdio lets the PTY
    /// reader EOF — the old "SIGKILL only if the reader has not exited" left it alive to write
    /// into the worktree after the guard's final snapshot.
    #[test]
    fn a_term_trapping_detached_descendant_dies_with_the_quiesced_session() {
        use std::os::unix::fs::PermissionsExt;
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
        let dir = std::env::temp_dir().join(format!(
            "wicked-sess-trap-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let late = dir.join("late.txt");
        let pidfile = dir.join("writer.pid");
        // A fake CLI NAMED codex: on every turn it backgrounds a TERM-immune, stdio-detached
        // writer that lands 3 s later, records its pid, then answers the turn.
        let codex = dir.join("codex");
        std::fs::write(
            &codex,
            format!(
                "#!/bin/sh\nwhile IFS= read -r line; do\n  nohup sh -c 'trap \"\" TERM; sleep 3; echo x >> \"{late}\"' </dev/null >/dev/null 2>&1 &\n  echo $! > \"{pid}\"\n  printf '{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"WKRTURN:%s\"}}]}}}}\\n' \"$line\"\n  printf '{{\"type\":\"result\",\"result\":\"ok\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}\\n'\ndone\n",
                late = late.display(),
                pid = pidfile.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&codex, std::fs::Permissions::from_mode(0o755)).unwrap();
        let invocation = format!("{} --sandbox workspace-write", codex.display());
        let (core, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();
        // A NO-CODE unit: its session is quiesced the moment the turn ends.
        let mut evaluator = make_unit("verify the fix", &invocation);
        evaluator.worktree_guarded = true;
        let out = runner.run_unit(&make_input("run-trap", 0, evaluator));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        wait_for(&events, |e| matches!(e, CoreEvent::TerminalExited { .. }));
        let pid: i32 = std::fs::read_to_string(&pidfile)
            .expect("the fixture recorded its writer's pid")
            .trim()
            .parse()
            .unwrap();
        // The writer is TERM-immune, so only the unconditional SIGKILL of the group explains its
        // death; give the kernel a moment to reap and the would-be write its full window.
        std::thread::sleep(std::time::Duration::from_millis(3500));
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(
            !alive,
            "the TERM-trapping writer (pid {pid}) survived the quiesce"
        );
        assert!(
            !late.exists(),
            "the detached writer must never land after the quiesce"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// core#396 (codex round 8, ADJUDICATED): the persistent PTY carrier does not load the skills
    /// snapshot, so a skill-bearing unit is REFUSED by name — naming the carrier and the skill —
    /// before any session is opened (no `TerminalOpened`), and no invocation directive is ever
    /// written to a PTY: a skill-free unit on the same run still runs, and the prompt the fake CLI
    /// echoes back carries no `Invoke your skill`.
    #[test]
    fn a_skill_bearing_unit_is_refused_on_the_pty_carrier_and_no_directive_is_written() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
        let (core, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();
        let invocation = fake_cli_invocation();
        let mut skilled = make_unit("extract the rules", &invocation);
        skilled.skill_ref = Some("wicked-garden-domain".to_string());
        let out = runner.run_unit(&make_input("run-pty-skills", 0, skilled));
        assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
        assert!(
            out.output
                .contains("persistent PTY sessions do not load the skills snapshot")
                && out.output.contains("wicked-garden-domain")
                && out.output.contains("wrapped or ACP carrier"),
            "refused by name, naming the carrier: {}",
            out.output
        );
        let mut opened = 0usize;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, CoreEvent::TerminalOpened { .. }) {
                opened += 1;
            }
        }
        assert_eq!(opened, 0, "a refused unit opens no session");
        // Skill-free: unchanged — the session opens, the turn runs, and no directive is written.
        let plain = make_unit("second work", &invocation);
        let out = runner.run_unit(&make_input("run-pty-skills", 1, plain));
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        assert!(
            out.output.contains("WKRTURN:") && !out.output.contains("Invoke your skill"),
            "the PTY prompt carries no skill directive: {}",
            out.output
        );
        runner.drop_session("run-pty-skills");
        wait_for(&events, |e| matches!(e, CoreEvent::TerminalExited { .. }));
    }

    /// codex round 9 (H3): the refusal is PLAN-WIDE. The actor hands every unit the run's whole
    /// skill set (`StepInput::required_skills`), so a run whose FIRST unit names no skill but whose
    /// later unit does is refused AT the first unit — before any session opens and before the fake
    /// CLI receives a single turn (its marker file is never created) — naming the carrier and the
    /// later unit's skill. Control: the same first unit in a run whose plan names no skill runs,
    /// and the marker proves the CLI received that turn.
    #[test]
    fn a_run_with_any_skill_bearing_unit_is_refused_at_its_first_unit_on_the_pty_carrier() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
        let (core, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        let events = core.subscribe();
        let marker = std::env::temp_dir().join(format!(
            "wicked-core-pty-marker-{}-{}",
            std::process::id(),
            DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&marker);
        let invocation = fake_cli_with_marker(&marker);
        // Unit 0 names no skill; the plan's later unit does.
        let mut input = make_input(
            "run-pty-plan",
            0,
            make_unit("first, skill-free work", &invocation),
        );
        input.required_skills = vec!["wicked-garden-domain".to_string()];
        let out = runner.run_unit(&input);
        assert_eq!(out.status, StepStatus::Failed, "{}", out.output);
        assert!(
            out.output
                .contains("persistent PTY sessions do not load the skills snapshot")
                && out.output.contains("wicked-garden-domain")
                && out.output.contains("wrapped or ACP carrier"),
            "refused at the first unit, naming the later unit's skill: {}",
            out.output
        );
        assert!(!marker.exists(), "the fake CLI never received a turn");
        let mut opened = 0usize;
        while let Ok(ev) = events.try_recv() {
            if matches!(ev, CoreEvent::TerminalOpened { .. }) {
                opened += 1;
            }
        }
        assert_eq!(opened, 0, "no session opens for a refused run");
        // Control: a run whose plan names no skill runs, and the marker records the turn.
        let plain = make_input(
            "run-pty-plain",
            0,
            make_unit("first, skill-free work", &invocation),
        );
        let out = runner.run_unit(&plain);
        assert_eq!(out.status, StepStatus::Ok, "{}", out.output);
        let turns = std::fs::read_to_string(&marker).expect("the fake CLI recorded the turn");
        assert!(!turns.trim().is_empty(), "{turns:?}");
        runner.drop_session("run-pty-plain");
        wait_for(&events, |e| matches!(e, CoreEvent::TerminalExited { .. }));
        let _ = std::fs::remove_file(&marker);
    }

    /// Two runs with DIFFERENT `run_id`s each open their own session.
    #[test]
    fn different_run_ids_open_separate_sessions() {
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
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
        let _env = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let _embedder = EmbedderPin::hash();
        let (_, runner) = crate::Core::spawn_with_pty_sessions(unique_db());
        runner.drop_session("no-such-run"); // must not panic
    }
}
