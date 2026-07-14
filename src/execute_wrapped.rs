//! WRAPPED-CLI execute backend (P4a) — the real [`StepRunner`] that runs an actual agentic CLI as a
//! subprocess in the run's worktree and captures its output. This is the organ that makes the
//! orchestrator *do real work* instead of returning a stub string.
//!
//! It implements ONLY the worker half (work production); the actor still owns the per-unit governance
//! gate + cursor + evidence (single-writer). The CLI is invoked **augment mode** (its own tools, no
//! hermetic lockdown). Per-tool-call PreToolUse governance (the gate-hook drain) is P4b — until then a
//! unit's output is governed at the unit level by the existing gate.
//!
//! Security: the prompt is passed as its OWN argv element with no shell (no command injection), with a
//! POSIX `--` end-of-options guard so a flag-shaped prompt can't smuggle a flag. Output is drained
//! CONCURRENTLY on threads while the child runs, so a verbose CLI exceeding the ~64KB pipe buffer can't
//! deadlock (the bug the P2 review flagged for this phase). The run is bounded by a timeout.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::domain::WorkUnit;
use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus, Usage};

/// The structured signals an [`OutputAdapter`] extracts from ONE raw stdout line (DES-STUDIO-COCKPIT-001
/// §3 B-runner). `text` is 0..n readable deltas to stream through the [`DeltaSink`] as `CliOutputDelta`
/// (never raw JSON — FR-2 live output stays prose); `usage` is the end-of-run token/cost total when the
/// line carried it; `files` are data-file paths the CLI touched (`tool_use.input.file_path`).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct AdapterOut {
    pub text: Vec<String>,
    pub usage: Option<Usage>,
    pub files: Vec<String>,
}

/// Per-CLI stdout adapter: turns a binary's raw stdout into readable deltas plus optional structured
/// signals (usage, files). Selected by the resolved binary — `claude` → [`ClaudeStreamJson`], everything
/// else → [`Passthrough`]. The default (passthrough) path is byte-identical to the pre-adapter behavior
/// (every line is one delta, no usage/files), so a non-claude run is unchanged.
pub trait OutputAdapter: Send {
    /// Consume one raw stdout line; return the readable deltas + any structured signals it carried.
    fn on_line(&mut self, line: &str) -> AdapterOut;
    /// Flush any buffered state when stdout closes (both current adapters are stateless → empty).
    fn finish(&mut self) -> AdapterOut {
        AdapterOut::default()
    }
}

/// Default adapter for every non-claude binary: each raw line is exactly one readable delta, no usage or
/// files. Byte-identical to the original raw-line streaming.
struct Passthrough;

impl OutputAdapter for Passthrough {
    fn on_line(&mut self, line: &str) -> AdapterOut {
        AdapterOut {
            text: vec![line.to_string()],
            usage: None,
            files: Vec::new(),
        }
    }
}

/// The claude `--output-format stream-json --verbose` NDJSON adapter (DES §6b, empirically grounded).
/// Per line: `assistant` `content[].type=="text"` → readable deltas; an `assistant` message whose
/// `content` is a bare STRING (not an array) → one text delta; `content[].type=="tool_use"` with
/// `input.file_path` → data files; `type=="result"` → `Usage` from `usage.input_tokens`/`output_tokens`
/// plus `cost_usd = total_cost_usd` (only when a `usage` object is present — no fabricated 0-token row).
/// FALLBACK (S3): if NO assistant text was emitted during the run, the terminal `result`'s `result`
/// string (the final answer) becomes the text delta, so `StepOutput.output` (the artifact the
/// creator≠evaluator judge reads) is never empty when the answer only arrives in the result envelope.
/// FAIL-SAFE: any line that is not valid JSON (version drift) degrades to a single passthrough text
/// delta, so it never panics and never blocks the run.
#[derive(Default)]
struct ClaudeStreamJson {
    /// Whether any assistant text delta was emitted this run — gates the terminal `result` fallback.
    emitted_text: bool,
}

impl OutputAdapter for ClaudeStreamJson {
    fn on_line(&mut self, line: &str) -> AdapterOut {
        if line.trim().is_empty() {
            return AdapterOut::default();
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            // version drift / non-JSON line → degrade to passthrough (fail-safe, never panic/block).
            Err(_) => {
                return AdapterOut {
                    text: vec![line.to_string()],
                    usage: None,
                    files: Vec::new(),
                };
            }
        };
        let mut out = AdapterOut::default();
        match v.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => {
                let content = v.get("message").and_then(|m| m.get("content"));
                match content {
                    // (S3a) `content` is a bare STRING (a valid-JSON shape that `as_array()` misses, so
                    // the text was silently dropped and — being valid JSON — never hit the passthrough
                    // fallback). Treat it as one readable text delta.
                    Some(serde_json::Value::String(s)) => {
                        if !s.is_empty() {
                            out.text.push(s.clone());
                        }
                    }
                    Some(serde_json::Value::Array(blocks)) => {
                        for block in blocks {
                            match block.get("type").and_then(|t| t.as_str()) {
                                // Readable prose → live-output delta (FR-2). Skip empty text blocks.
                                Some("text") => {
                                    if let Some(t) = block.get("text").and_then(|t| t.as_str()) {
                                        if !t.is_empty() {
                                            out.text.push(t.to_string());
                                        }
                                    }
                                }
                                // A tool call touching a file (Read/Edit/Write/…) → a data-in-use signal (B4).
                                Some("tool_use") => {
                                    if let Some(fp) = block
                                        .get("input")
                                        .and_then(|i| i.get("file_path"))
                                        .and_then(|f| f.as_str())
                                    {
                                        if !fp.is_empty() {
                                            out.files.push(fp.to_string());
                                        }
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
                if !out.text.is_empty() {
                    self.emitted_text = true;
                }
            }
            // The terminal result carries the run totals + cost directly (B3).
            Some("result") => {
                // (M8) Only synthesize `Usage` when a `usage` object is actually present. A missing
                // `usage` must leave `usage = None` so NO `CliUsage` row is emitted for the unit — never
                // a fabricated "$0.00, 0 tokens" total.
                if let Some(usage) = v.get("usage") {
                    let input_tokens = usage
                        .get("input_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0);
                    let output_tokens = usage
                        .get("output_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0);
                    let cost_usd = v.get("total_cost_usd").and_then(|c| c.as_f64());
                    out.usage = Some(Usage {
                        input_tokens,
                        output_tokens,
                        cost_usd,
                    });
                }
                // (S3b) FALLBACK: no assistant text streamed this run ⇒ the final answer lives only in the
                // result envelope. Emit `result.result` as text so `StepOutput.output` is non-empty.
                if !self.emitted_text {
                    if let Some(answer) = v.get("result").and_then(|r| r.as_str()) {
                        if !answer.is_empty() {
                            out.text.push(answer.to_string());
                            self.emitted_text = true;
                        }
                    }
                }
            }
            // system / user / rate_limit_event / anything else → no readable output, no signals.
            _ => {}
        }
        out
    }
}

/// Whether the resolved binary is `claude` (selects the stream-json adapter + flag injection). Matches on
/// the file stem so `claude`, `/usr/local/bin/claude`, and `claude.exe` (Windows) all resolve.
///
/// CLAUDE-ADAPTER CONTRACT (known boundary): the stream-json adapter is selected purely by binary stem
/// (`stem == "claude"`); a claude-compatible binary under a different name is NOT recognized (M7). The
/// operator template MUST run claude in print/headless mode (`-p`/`--print`) — that is the mode under
/// which `--output-format stream-json --verbose` emits the NDJSON this adapter parses; without it claude
/// runs interactively and the adapter degrades to passthrough. (M9: a raw stdout line containing invalid
/// UTF-8 is dropped by the `map_while(Result::ok)` line reader — a pre-existing, accepted boundary.)
fn binary_is_claude(bin: &str) -> bool {
    std::path::Path::new(bin)
        .file_stem()
        .map(|s| s == "claude")
        .unwrap_or(false)
}

/// Whether this unit's resolved invocation runs `claude` (the binary input governance targets). Mirrors
/// `exec`'s `is_claude` decision — the first argv token after resolving the template — so the actor-side
/// fold can independently determine a unit WAS governed (a claude unit on a file-backed store), which
/// gates evidence-integrity fail-closure without threading a flag through `StepOutput`.
pub(crate) fn unit_uses_claude(unit: &WorkUnit) -> bool {
    let cli_key = unit.assigned_cli.as_deref().unwrap_or("claude");
    let invocation = unit
        .assigned_invocation
        .clone()
        .unwrap_or_else(|| resolve_invocation(cli_key));
    tokenize(&invocation)
        .first()
        .map(|b| binary_is_claude(b))
        .unwrap_or(false)
}

/// Append claude's `--output-format stream-json --verbose` flags to an already-built argv, INSERTED
/// before any `--` end-of-options guard so they are parsed as flags (never demoted to positional args
/// after the prompt). Per-binary rule — only applied when the resolved binary is `claude`; no other
/// seat's template is touched.
fn inject_claude_stream_flags(argv: &mut Vec<String>) {
    // (M6) Skip injection when the operator template already sets `--output-format` (e.g.
    // `--output-format json`): injecting a SECOND `--output-format stream-json` produces conflicting
    // flags that claude rejects, failing the run. Honor the template's choice as-is.
    if argv.iter().any(|a| a == "--output-format") {
        return;
    }
    let flags = ["--output-format", "stream-json", "--verbose"];
    match argv.iter().position(|a| a == "--") {
        Some(i) => {
            for (k, f) in flags.iter().enumerate() {
                argv.insert(i + k, f.to_string());
            }
        }
        None => argv.extend(flags.iter().map(|f| f.to_string())),
    }
}

/// The real wrapped-CLI runner. Resolves each unit's assigned CLI to its invocation template, runs it
/// in the unit's worktree, and maps the exit code to a [`StepStatus`].
pub struct WrappedCliStepRunner {
    /// Per-unit wall-clock bound. A CLI exceeding it is killed and the step reports `Cancelled`.
    timeout: Duration,
}

impl Default for WrappedCliStepRunner {
    fn default() -> Self {
        WrappedCliStepRunner {
            timeout: Duration::from_secs(900),
        }
    }
}

impl StepRunner for WrappedCliStepRunner {
    fn run_unit(&self, input: &StepInput) -> StepOutput {
        // No live sink → a no-op emit (non-streaming callers).
        let noop = |_: &str| {};
        self.exec(input, &noop)
    }

    fn run_unit_streaming(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        self.exec(input, emit)
    }
}

impl WrappedCliStepRunner {
    /// Run one unit's CLI, streaming stdout lines through `emit` as they arrive.
    fn exec(&self, input: &StepInput, emit: &DeltaSink) -> StepOutput {
        let cli_key = input
            .unit
            .assigned_cli
            .as_deref()
            .unwrap_or("claude")
            .to_string();
        // Prefer the unit's own invocation template (an ad-hoc launch CLI not in the registry); else
        // resolve the key via the council registry.
        let invocation = input
            .unit
            .assigned_invocation
            .clone()
            .unwrap_or_else(|| resolve_invocation(&cli_key));
        let mut argv = build_argv(
            &invocation,
            &skill_prompt(&input.unit),
            &input.unit.allowed_skills,
        );

        // Per-binary output adapter (B-runner). claude → stream-json (+ the two flags, injected before the
        // `--` guard); every other binary → passthrough (byte-identical to the pre-adapter raw-line stream).
        let is_claude = argv.first().map(|a| binary_is_claude(a)).unwrap_or(false);
        if is_claude {
            inject_claude_stream_flags(&mut argv);
        }

        // GOVERNED unit + claude → arm INPUT governance (DES-OUTGOV-003 §2): write a per-run settings
        // file declaring a PreToolUse gate-hook (every tool; exit 2 = deny ⇒ claude aborts the call),
        // insert `--settings <file>`, and return the child env (decisions log + absolute store path).
        // `--settings` MERGES (the user's own settings stay intact) and lives OUTSIDE the worktree.
        // Non-claude CLIs + ungoverned internal calls (`governance: None`) are untouched.
        let gov_env: Option<GovLaunch> = match (&input.governance, is_claude) {
            (Some(gov), true) => match arm_input_governance(input, gov, &mut argv) {
                Ok(env) => Some(env),
                // A governed unit whose governance cannot be armed must NOT run ungoverned — fail it.
                Err(e) => {
                    return StepOutput {
                        run_id: input.run_id.clone(),
                        unit_ix: input.unit_ix,
                        attempt: input.attempt,
                        output: format!("(could not arm input governance: {e})"),
                        status: StepStatus::Failed,
                        usage: None,
                        files: Vec::new(),
                    }
                }
            },
            _ => None,
        };

        // Run in the worktree if the run targets a repo; else a per-run temp sandbox (never the
        // orchestrator's own cwd).
        let cwd = input.workdir.clone().unwrap_or_else(|| sandbox_for(input));
        let _ = std::fs::create_dir_all(&cwd);

        let (status, output, usage, files) = if argv.is_empty() {
            (
                StepStatus::Failed,
                format!("(no invocation configured for cli `{cli_key}`)"),
                None,
                Vec::new(),
            )
        } else {
            let adapter: Box<dyn OutputAdapter> = if is_claude {
                Box::<ClaudeStreamJson>::default()
            } else {
                Box::new(Passthrough)
            };
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(&cwd);
            // The gate-hook subprocess (spawned by claude) reads these: the append-only decisions log,
            // the absolute estate store path, and the unit's scope/phase. Scope/phase travel via ENV
            // (NOT interpolated into the shell hook command) so caller-controlled ids can never inject
            // shell metacharacters — the command string carries only the trusted exe (DES-OUTGOV-003 §8).
            if let Some(g) = &gov_env {
                cmd.env(crate::gate_hook::DECISIONS_PATH_ENV, &g.decisions_path);
                cmd.env(crate::gate_hook::ESTATE_DB_ENV, &g.db_path);
                cmd.env(crate::gate_hook::GATE_SCOPE_ENV, &g.scope);
                cmd.env(crate::gate_hook::GATE_PHASE_ENV, &g.phase);
            }
            match run_bounded(cmd, self.timeout, emit, adapter) {
                Ok((0, out, _, usage, files)) => (StepStatus::Ok, out, usage, files),
                Ok((-1, _, err, _, _)) if err == TIMED_OUT => (
                    StepStatus::Cancelled,
                    format!("(cli `{cli_key}` exceeded the timeout and was killed)"),
                    None,
                    Vec::new(),
                ),
                Ok((code, out, err, _, _)) => {
                    let detail = if !out.trim().is_empty() { out } else { err };
                    (
                        StepStatus::Failed,
                        format!("(cli `{cli_key}` exited {code}) {detail}"),
                        None,
                        Vec::new(),
                    )
                }
                Err(e) => (
                    StepStatus::Failed,
                    format!("(could not run `{}`: {e})", argv[0]),
                    None,
                    Vec::new(),
                ),
            }
        };

        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output,
            status,
            usage,
            files,
        }
    }
}

/// A per-run temp sandbox for repo-less runs (so a real CLI never edits the orchestrator's own tree).
fn sandbox_for(input: &StepInput) -> PathBuf {
    std::env::temp_dir()
        .join("wicked-core-sandbox")
        .join(&input.run_id)
}

/// The `PreToolUse` hook command — the exe path quoted for the platform's shell so a `$`/backtick/space
/// in the install path can't be expanded or split. POSIX single-quotes disable all expansion (with the
/// standard `'\''` escape for an embedded quote); Windows cmd double-quotes (it does not expand `$`).
fn quote_exe_command(exe: &str) -> String {
    #[cfg(unix)]
    {
        format!("'{}' gate-hook", exe.replace('\'', "'\\''"))
    }
    #[cfg(not(unix))]
    {
        format!("\"{exe}\" gate-hook")
    }
}

/// What the launcher sets on the wrapped CLI's `Command` to arm INPUT governance. Scope/phase are set as
/// ENV (never interpolated into the shell hook command) so caller-controlled ids cannot inject shell
/// metacharacters.
struct GovLaunch {
    decisions_path: PathBuf,
    db_path: String,
    scope: String,
    phase: String,
}

/// Arm INPUT governance for a governed claude unit (DES-OUTGOV-003 §2): derive the unit's REAL
/// `resolve_scope(...)` / `unit-{ord}` (so the hook's policy `select` + the recorded `claim.phase` match
/// the run engine's own per-unit gate, findings #1/#7), write a per-(unit,attempt) `--settings` file
/// declaring the `PreToolUse` gate-hook, insert `--settings <file>` into `argv` (before the prompt / any
/// `--` guard), and return the env the launcher sets on the child. Settings + decisions live under a
/// per-run/attempt dir OUTSIDE the worktree (no repo pollution).
///
/// SECURITY: the hook command is a CONSTANT (`"<exe>" gate-hook`) — only the trusted `current_exe()` is
/// interpolated (double-quoted for spaces; it contains no shell metacharacters). Scope/phase (which
/// embed the caller-controlled `session_id`/`unit.id`) travel via `WICKED_GATE_SCOPE`/`WICKED_GATE_PHASE`
/// env, so no attacker-controlled data ever reaches the shell-executed command string — closing the
/// injection / fail-open hole a naive double-quoted argv would open.
fn arm_input_governance(
    input: &StepInput,
    gov: &crate::workflow::GovernanceContext,
    argv: &mut Vec<String>,
) -> std::io::Result<GovLaunch> {
    let scope = crate::scope::resolve_scope(input.entity_mode, &input.run_id, &input.unit.id);
    let phase = crate::scope::unit_phase(input.unit.ord);
    let decisions_path = crate::gate_hook::decisions_path_for(&input.run_id, input.attempt);
    let exe = std::env::current_exe()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "wicked-core".to_string());
    // exit 2 = deny ⇒ claude aborts the tool-call; matcher "*" governs EVERY tool. Only the exe is
    // interpolated (scope/phase go via env). Quote it per-platform so a `$`/backtick in the install path
    // can't be shell-expanded (POSIX single-quote disables ALL expansion; on Windows cmd `$`/backtick are
    // not special, so double-quote for spaces — a `"` is illegal in a Windows path anyway).
    let command = quote_exe_command(&exe);
    let settings = serde_json::json!({
        "hooks": {
            "PreToolUse": [
                { "matcher": "*", "hooks": [ { "type": "command", "command": command } ] }
            ]
        }
    });
    let dir = decisions_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("wicked-core-gov"));
    crate::gate_hook::create_dir_all_private(&dir)?;
    // Per-unit settings file. Written with `create_new` (O_EXCL) so a local attacker who predicts the
    // deterministic temp path can't pre-place a symlink and redirect the write (council [6] TOCTOU); a
    // clash means either a re-arm of the same unit or an attack — either way fail closed by erroring.
    let settings_path = dir.join(format!("settings-{phase}.json"));
    let bytes = serde_json::to_vec(&settings).map_err(std::io::Error::other)?;
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&settings_path)
            .or_else(|e| {
                // Tolerate a legitimate re-arm (same unit, same attempt) by truncating our OWN prior file.
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .truncate(true)
                        .open(&settings_path)
                } else {
                    Err(e)
                }
            })?;
        f.write_all(&bytes)?;
    }
    // Write the ARMED marker BEFORE the CLI runs: its presence lets the actor-side fold distinguish a
    // governed unit that legitimately made no tool-calls (marker only) from one whose evidence was erased
    // or whose hook never fired (marker absent → fail closed). Closes the council evidence-integrity blocker.
    crate::gate_hook::write_armed_marker(&decisions_path, &phase)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    // Insert `--settings <path>` right after the binary so it parses as a flag (never demoted past the
    // prompt / a `--` guard).
    argv.insert(1, settings_path.to_string_lossy().into_owned());
    argv.insert(1, "--settings".to_string());
    Ok(GovLaunch {
        decisions_path,
        db_path: gov.db_path.clone(),
        scope,
        phase,
    })
}

/// Resolve a CLI key to its headless invocation template. Reads the council registry (built-ins +
/// the user's `~/.config/wicked-council/clis.toml`); if the key isn't registered, treats the key
/// itself as the binary (`<key> {PROMPT}`) so an ad-hoc binary still runs.
fn resolve_invocation(cli_key: &str) -> String {
    let user =
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/wicked-council/clis.toml"));
    if let Ok(clis) = wicked_council::registry::load(user.as_deref()) {
        if let Some(c) = clis.iter().find(|c| c.key == cli_key) {
            if !c.headless_invocation.trim().is_empty() {
                return c.headless_invocation.clone();
            }
        }
    }
    format!("{cli_key} {{PROMPT}}")
}

const TIMED_OUT: &str = "__wicked_timed_out__";

/// The outcome of a bounded run: `(exit_code, stdout, stderr, usage, files)`.
type BoundedRun = (i32, String, String, Option<Usage>, Vec<String>);

/// Run `cmd` bounded by `timeout`, draining stdout+stderr CONCURRENTLY (no pipe-buffer deadlock). Each
/// raw stdout line is routed through `adapter`, whose READABLE text deltas are streamed through `emit`
/// (live output) exactly as raw lines were before (for passthrough) while its structured signals (usage,
/// files) are accumulated. Returns `(exit_code, stdout, stderr, usage, files)`; a timeout returns
/// `(-1, "", TIMED_OUT, None, [])` after killing. Uses a scoped thread so the stdout drain can borrow
/// `emit` (which lives on the worker stack); the adapter is MOVED into that thread.
fn run_bounded(
    mut cmd: Command,
    timeout: Duration,
    emit: &DeltaSink,
    mut adapter: Box<dyn OutputAdapter>,
) -> std::io::Result<BoundedRun> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let so = child.stdout.take().expect("piped stdout");
    let se = child.stderr.take().expect("piped stderr");
    let child_ref = &mut child;

    // Cap the ACCUMULATED buffers so a runaway/verbose CLI can't OOM the orchestrator. Streaming via
    // `emit` is unaffected (every delta still streams); only the retained string is bounded.
    const MAX_OUT: usize = 8 * 1024 * 1024;

    let (code, timed_out, out, usage, files, err) = std::thread::scope(|scope| {
        // Stdout: read line-by-line, route through `adapter`, stream each readable delta through `emit`,
        // accumulate the readable text (bounded) + the structured signals (usage/files).
        let out_h = scope.spawn(move || {
            use std::io::BufRead;
            let mut s = String::new();
            let mut capped = false;
            let mut usage: Option<Usage> = None;
            let mut files: Vec<String> = Vec::new();
            let mut absorb = |ao: AdapterOut, s: &mut String, capped: &mut bool| {
                for t in ao.text {
                    emit(&t);
                    if s.len() < MAX_OUT {
                        s.push_str(&t);
                        s.push('\n');
                    } else if !*capped {
                        s.push_str("\n… (output truncated)\n");
                        *capped = true;
                    }
                }
                if ao.usage.is_some() {
                    usage = ao.usage;
                }
                files.extend(ao.files);
            };
            for line in std::io::BufReader::new(so).lines().map_while(Result::ok) {
                let ao = adapter.on_line(&line);
                absorb(ao, &mut s, &mut capped);
            }
            let fin = adapter.finish();
            absorb(fin, &mut s, &mut capped);
            (s, usage, files)
        });
        let err_h = scope.spawn(move || {
            let mut s = String::new();
            // Bounded read so a verbose stderr can't OOM either.
            let _ = se.take(MAX_OUT as u64).read_to_string(&mut s);
            s
        });

        let start = Instant::now();
        let (code, timed_out) = loop {
            match child_ref.try_wait() {
                Ok(Some(status)) => break (status.code().unwrap_or(-1), false),
                Ok(None) => {
                    if start.elapsed() > timeout {
                        let _ = child_ref.kill();
                        let _ = child_ref.wait();
                        break (-1, true);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(_) => break (-1, false),
            }
        };
        let (out, usage, files) = out_h.join().unwrap_or_default();
        let err = err_h.join().unwrap_or_default();
        (code, timed_out, out, usage, files, err)
    });

    if timed_out {
        // Preserve what the CLI produced before the kill (debugging context on a hang).
        Ok((-1, out, TIMED_OUT.to_string(), usage, files))
    } else {
        Ok((code, out, err, usage, files))
    }
}

// ── argv building (ported from the proven UI logic — no shell, `--` guard) ──────────────────────

/// Whitespace tokenizer that keeps double-quoted spans together and strips the quotes.
fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    let mut any = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                any = true;
            }
            c if c.is_whitespace() && !in_quote => {
                if any {
                    out.push(std::mem::take(&mut cur));
                    any = false;
                }
            }
            c => {
                cur.push(c);
                any = true;
            }
        }
    }
    if any {
        out.push(cur);
    }
    out
}

/// Build a no-shell argv from an invocation template, placing the untrusted `prompt` as its OWN argv
/// element. A POSIX `--` end-of-options guard is inserted before a positional prompt so a flag-shaped
/// prompt can't smuggle a flag; when `{PROMPT}` is a flag's value (preceding token is an option) the
/// binding is preserved and no `--` is added.
/// The prompt for a unit's CLI invocation. When the unit is **skill-driven** (DES-EXEC-001 §4.1), the
/// prompt LEADS with the leading-slash form `/{skill_ref} {description}` so the harness expands the
/// named skill (spike-verified for `claude` **given the skill is installed** in `~/.claude/skills/` —
/// see brain `headless-skill-invocation-recipe`); otherwise it's the bare description (authored path).
///
/// LIMITATION (Law-2): `/{skill}` is the **Claude-Code** slash-command form. Other CLIs express "run
/// this skill" differently, so the per-CLI skill form should become template data (like `{SKILLS}`)
/// rather than this hard-coded prefix — tracked as a follow-up. Today only the claude form is grounded.
/// Pure + testable without a subprocess.
///
/// The runtime skill allowlist (`unit.allowed_skills`, §4.2) rides the invocation template via a
/// `{SKILLS}` placeholder — see [`build_argv`]. The template author picks the per-CLI flag (e.g.
/// `claude … --allowedTools {SKILLS}`), so the engine never hard-codes one CLI's semantics.
pub(crate) fn skill_prompt(unit: &WorkUnit) -> String {
    match unit.skill_ref.as_deref() {
        Some(skill) if !skill.is_empty() => format!("/{skill} {}", unit.description),
        _ => unit.description.clone(),
    }
}

/// Build the argv from an invocation template, substituting `{PROMPT}` (the skill-led prompt, guarded
/// as its own arg) and `{SKILLS}` (the runtime allowlist, §4.2).
///
/// The allowlist rides a **glued** token — e.g. `--allowedTools={SKILLS}`. When the allowlist is
/// non-empty the placeholder is replaced with the comma-joined skills; when EMPTY the whole token is
/// dropped (the flag disappears with no dangling empty value). The substitution is inserted **before**
/// any `--` end-of-options guard, so the flag can never be demoted to a positional arg even if the
/// template places it after `{PROMPT}`. Unlike the earlier heuristic, nothing pops a *preceding* token,
/// so an unrelated flag can never be silently deleted. (A bare `{SKILLS}` token also works — it expands
/// in place — but only the glued form elides its flag cleanly when the allowlist is empty.)
pub(crate) fn build_argv(invocation: &str, prompt: &str, skills: &[String]) -> Vec<String> {
    let toks = tokenize(invocation);
    let mut argv: Vec<String> = Vec::new();
    let mut placed = false;
    let joined = skills.join(",");
    let ensure_guard = |argv: &mut Vec<String>| {
        // A bare flag (`-p`, `--foo`) may take the prompt as its value ⇒ no guard. A GLUED flag
        // (`--foo=bar`) is self-contained ⇒ the prompt is NOT its value, so it still needs the guard.
        let prev_is_flag = argv
            .last()
            .map(|p| p.starts_with('-') && !p.contains('='))
            .unwrap_or(false);
        if !prev_is_flag && !argv.iter().any(|a| a == "--") {
            argv.push("--".to_string());
        }
    };
    // Insert a skills arg BEFORE any already-pushed `--` guard (keeps flags out of positional land).
    let insert_pre_guard =
        |argv: &mut Vec<String>, arg: String| match argv.iter().position(|a| a == "--") {
            Some(i) => argv.insert(i, arg),
            None => argv.push(arg),
        };
    for t in &toks {
        if t.contains("{SKILLS}") {
            // Empty allowlist ⇒ drop the whole token (flag + value vanish). Non-empty ⇒ substitute.
            if !skills.is_empty() {
                insert_pre_guard(&mut argv, t.replace("{SKILLS}", &joined));
            }
        } else if t == "{PROMPT}" {
            ensure_guard(&mut argv);
            argv.push(prompt.to_string());
            placed = true;
        } else if t.contains("{PROMPT}") {
            argv.push(t.replace("{PROMPT}", prompt));
            placed = true;
        } else {
            argv.push(t.clone());
        }
    }
    if !placed {
        ensure_guard(&mut argv);
        argv.push(prompt.to_string());
    }
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn run_bounded_streams_each_stdout_line_live() {
        let lines = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let emit = move |line: &str| sink.lock().unwrap().push(line.to_string());
        let mut cmd = Command::new("printf");
        cmd.arg("alpha\nbeta\ngamma\n");
        let (code, out, _err, _usage, _files) =
            run_bounded(cmd, Duration::from_secs(5), &emit, Box::new(Passthrough)).unwrap();
        assert_eq!(code, 0);
        assert_eq!(
            *lines.lock().unwrap(),
            vec!["alpha", "beta", "gamma"],
            "each stdout line is streamed through emit as it arrives"
        );
        assert!(
            out.contains("alpha") && out.contains("gamma"),
            "the full output is still accumulated alongside streaming"
        );
    }

    #[test]
    fn arm_input_governance_writes_a_pretool_settings_file_and_returns_env() {
        let mut u = WorkUnit::pending("s:u1", "s", 3, "do it");
        u.assigned_cli = Some("claude".to_string());
        let gov = crate::workflow::GovernanceContext {
            db_path: "/abs/estate.db".to_string(),
        };
        let input = StepInput {
            run_id: format!("armtest-{}", std::process::id()),
            unit_ix: 0,
            attempt: 0,
            unit: u,
            workflow_id: "wf-x".to_string(),
            entity_mode: crate::scope::EntityMode::Isolated,
            workdir: None,
            governance: Some(gov.clone()),
        };
        let mut argv = vec!["claude".to_string(), "-p".to_string(), "hi".to_string()];
        let g = arm_input_governance(&input, &gov, &mut argv).unwrap();

        assert_eq!(g.db_path, "/abs/estate.db", "the child gets the store path");
        // scope/phase ride the RETURNED struct (→ env), pinned to the unit's real values.
        assert_eq!(g.phase, "unit-3", "phase pinned to the unit's real ord");
        assert!(
            g.scope.starts_with("wicked-agent/"),
            "scope pinned to resolve_scope: {}",
            g.scope
        );
        // `--settings <file>` inserted right after the binary (parses as a flag, before the prompt).
        assert_eq!(argv[0], "claude");
        assert_eq!(argv[1], "--settings");
        let settings_path = std::path::PathBuf::from(&argv[2]);
        assert!(settings_path.exists(), "the settings file was written");
        assert!(
            settings_path.starts_with(std::env::temp_dir()),
            "settings live OUTSIDE any worktree (no repo pollution): {settings_path:?}"
        );
        assert!(
            g.decisions_path.starts_with(std::env::temp_dir()),
            "the decisions log lives outside any worktree"
        );

        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(
            json["hooks"]["PreToolUse"][0]["matcher"], "*",
            "the hook governs EVERY tool"
        );
        let cmd = json["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert!(
            cmd.contains("gate-hook"),
            "runs the gate-hook subcommand: {cmd}"
        );
        // SECURITY: the command carries NO caller-controlled data — scope/phase go via env, not the shell
        // string. Only the (trusted, double-quoted) exe is interpolated.
        assert!(
            !cmd.contains("--scope") && !cmd.contains("--phase") && !cmd.contains("--db"),
            "no scope/phase/db interpolated into the shell-executed hook command: {cmd}"
        );
        assert!(
            !cmd.contains("wicked-agent/"),
            "the caller-controlled scope must NOT appear in the shell command: {cmd}"
        );
        let q = if cfg!(unix) { '\'' } else { '"' };
        assert!(
            cmd.trim_start().starts_with(q),
            "the exe path is quoted per-platform ({q}) so $/backtick/space can't be expanded: {cmd}"
        );
        let _ = std::fs::remove_dir_all(gov_run_dir_for_test(&input.run_id));
    }

    // The gov run dir for cleanup — mirrors gate_hook::gov_run_dir without exposing it beyond the crate.
    fn gov_run_dir_for_test(run_id: &str) -> std::path::PathBuf {
        crate::gate_hook::gov_run_dir(run_id)
    }

    #[test]
    fn skill_prompt_leads_with_the_headless_slash_form() {
        let mut u = WorkUnit::pending("s:build", "s", 1, "add SSO login");
        // authored path: no skill → bare description.
        assert_eq!(skill_prompt(&u), "add SSO login");
        // skill-driven: leads with /<skill> so the harness expands the named skill deterministically.
        u.skill_ref = Some("wicked-testing-semantic-reviewer".to_string());
        assert_eq!(
            skill_prompt(&u),
            "/wicked-testing-semantic-reviewer add SSO login"
        );
        // an empty skill_ref is treated as no skill (authored path), never a bare "/ ...".
        u.skill_ref = Some(String::new());
        assert_eq!(skill_prompt(&u), "add SSO login");
    }

    #[test]
    fn a_skill_prompt_flows_through_build_argv_as_one_guarded_arg() {
        let mut u = WorkUnit::pending("s:build", "s", 1, "do it");
        u.skill_ref = Some("wicked-testing-plan".to_string());
        let argv = build_argv("claude -p {PROMPT}", &skill_prompt(&u), &[]);
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "-p".to_string(),
                "/wicked-testing-plan do it".to_string(),
            ],
            "the skill-led prompt binds as -p's value, one argv element (no shell, no flag smuggling)"
        );
    }

    #[test]
    fn skills_placeholder_expands_the_glued_allowlist_flag() {
        let skills = vec![
            "wicked-testing-execution".to_string(),
            "wicked-testing-authoring".to_string(),
        ];
        let argv = build_argv(
            "claude -p --allowedTools={SKILLS} {PROMPT}",
            "do it",
            &skills,
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "-p".to_string(),
                "--allowedTools=wicked-testing-execution,wicked-testing-authoring".to_string(),
                "--".to_string(),
                "do it".to_string(),
            ],
            "the glued flag carries the comma-joined allowlist; the prompt still gets its -- guard"
        );
    }

    #[test]
    fn empty_skills_drop_the_whole_glued_flag_token() {
        // No allowlist ⇒ the entire `--allowedTools={SKILLS}` token vanishes (no dangling flag).
        let argv = build_argv("claude -p --allowedTools={SKILLS} {PROMPT}", "do it", &[]);
        assert_eq!(
            argv,
            vec!["claude".to_string(), "-p".to_string(), "do it".to_string()]
        );
    }

    #[test]
    fn skills_after_prompt_still_land_before_the_guard() {
        // Even a misordered template ({SKILLS} after {PROMPT}) must not demote the flag past `--`.
        // `run {PROMPT}` gives the prompt a `--` guard (prev token isn't a value-taking flag); the
        // later skills flag must be inserted BEFORE that guard.
        let skills = vec!["a".to_string()];
        let argv = build_argv(
            "claude run {PROMPT} --allowedTools={SKILLS}",
            "do it",
            &skills,
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "run".to_string(),
                "--allowedTools=a".to_string(),
                "--".to_string(),
                "do it".to_string(),
            ],
            "the allowlist flag is inserted before the -- guard, never left in positional territory"
        );
    }

    #[test]
    fn no_unrelated_preceding_flag_is_ever_deleted() {
        // Regression for the old pop-heuristic: an empty allowlist must NOT delete an adjacent flag
        // that isn't the allowlist flag. With the glued form there is no preceding-token pop at all.
        let argv = build_argv(
            "claude --verbose --allowedTools={SKILLS} -p {PROMPT}",
            "go",
            &[],
        );
        assert_eq!(
            argv,
            vec![
                "claude".to_string(),
                "--verbose".to_string(),
                "-p".to_string(),
                "go".to_string(),
            ],
            "--verbose survives; only the glued allowlist token is dropped"
        );
    }

    #[test]
    fn prompt_is_a_guarded_standalone_arg() {
        assert_eq!(
            build_argv("echo {PROMPT}", "--help", &[]),
            vec!["echo".to_string(), "--".to_string(), "--help".to_string()]
        );
    }

    #[test]
    fn flag_value_position_keeps_binding() {
        assert_eq!(
            build_argv("claude -p {PROMPT}", "hi", &[]),
            vec!["claude".to_string(), "-p".to_string(), "hi".to_string()]
        );
    }

    #[test]
    fn unknown_cli_falls_back_to_key_as_binary() {
        // A key not in the registry becomes `<key> {PROMPT}`.
        let inv = resolve_invocation("definitely-not-a-registered-cli-xyz");
        assert_eq!(inv, "definitely-not-a-registered-cli-xyz {PROMPT}");
    }

    // ── B-runner adapters (DES-STUDIO-COCKPIT-001 §3 / §6b) ──────────────────────────────────────────

    /// Drive an adapter over a slice of raw lines the way `run_bounded`'s stdout drain does: collect the
    /// readable deltas in order, keep the LAST usage seen, and accumulate every file path.
    fn drive(adapter: &mut dyn OutputAdapter, lines: &[&str]) -> AdapterOut {
        let mut acc = AdapterOut::default();
        let mut absorb = |ao: AdapterOut| {
            acc.text.extend(ao.text);
            if ao.usage.is_some() {
                acc.usage = ao.usage;
            }
            acc.files.extend(ao.files);
        };
        for l in lines {
            absorb(adapter.on_line(l));
        }
        absorb(adapter.finish());
        acc
    }

    /// Faithful structural slice of the empirical `/tmp/cj.ndjson` capture (DES §6b): a system init, an
    /// assistant `thinking` block (no readable text), an assistant `tool_use` Read carrying `file_path`,
    /// a `rate_limit_event`, an assistant `text` block, and the terminal `result` with `usage` +
    /// `total_cost_usd`. Values (tokens 25789/83, cost 0.409099, path, text) are the measured ones.
    const CLAUDE_FIXTURE: &[&str] = &[
        r#"{"type":"system","subtype":"init","session_id":"d2a386ef-958b-4f5f-984c-3bce7238bb30"}"#,
        r#"{"type":"assistant","message":{"model":"claude-opus-4-8","role":"assistant","content":[{"type":"thinking","thinking":""}]}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"toolu_01VsZS6YPmvMhjD4TD82Lh2T","name":"Read","input":{"file_path":"/tmp/wc-probe.txt"}}]}}"#,
        r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#,
        r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"#,
        r#"{"type":"result","subtype":"success","is_error":false,"result":"hello","total_cost_usd":0.409099,"usage":{"input_tokens":25789,"cache_creation_input_tokens":26103,"cache_read_input_tokens":34098,"output_tokens":83}}"#,
    ];

    #[test]
    fn t_d1_claude_adapter_extracts_text_usage_and_files() {
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(&mut adapter, CLAUDE_FIXTURE);
        // Readable prose only — no raw JSON leaked to FR-2 live output; thinking/system/result yield none.
        assert_eq!(
            out.text,
            vec!["hello".to_string()],
            "only assistant text blocks become readable deltas"
        );
        // Usage from the terminal result: tokens + cost DIRECTLY from claude (no price table needed).
        assert_eq!(
            out.usage,
            Some(Usage {
                input_tokens: 25789,
                output_tokens: 83,
                cost_usd: Some(0.409099),
            })
        );
        // Files from the tool_use `input.file_path`.
        assert_eq!(out.files, vec!["/tmp/wc-probe.txt".to_string()]);
    }

    #[test]
    fn t_d1_claude_adapter_degrades_malformed_line_to_passthrough_no_panic() {
        let mut adapter = ClaudeStreamJson::default();
        // A non-JSON line (version drift) must degrade to a single passthrough text delta, never panic.
        let out = adapter.on_line("not json at all {oops");
        assert_eq!(out.text, vec!["not json at all {oops".to_string()]);
        assert!(out.usage.is_none());
        assert!(out.files.is_empty());
        // And a run mixing a garbage line with good lines still recovers the real usage/files.
        let mut adapter = ClaudeStreamJson::default();
        let mut lines = vec!["}{ broken"];
        lines.extend_from_slice(CLAUDE_FIXTURE);
        let out = drive(&mut adapter, &lines);
        assert_eq!(out.files, vec!["/tmp/wc-probe.txt".to_string()]);
        assert!(out.usage.is_some());
        assert!(out.text.contains(&"}{ broken".to_string()));
        assert!(out.text.contains(&"hello".to_string()));
    }

    #[test]
    fn t_d1_claude_adapter_string_content_becomes_a_text_delta() {
        // (S3a) An `assistant` message whose `content` is a bare STRING (not an array) is valid JSON, so
        // it never hit the passthrough fallback — and `as_array()` returned None, silently DROPPING the
        // text. It must now surface as one readable delta.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[
                r#"{"type":"assistant","message":{"role":"assistant","content":"the answer is 42"}}"#,
            ],
        );
        assert_eq!(
            out.text,
            vec!["the answer is 42".to_string()],
            "string content is emitted as a text delta (no longer dropped)"
        );
    }

    #[test]
    fn t_d1_claude_result_only_answer_yields_nonempty_output() {
        // (S3b) When NO assistant text streamed (the answer arrives only in the terminal `result`
        // envelope), the `result.result` string is emitted as text so `StepOutput.output` — the artifact
        // the creator≠evaluator judge reads — is never empty (an empty artifact → spurious reject).
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[
                r#"{"type":"system","subtype":"init"}"#,
                r#"{"type":"result","subtype":"success","is_error":false,"result":"FINAL ANSWER","total_cost_usd":0.01,"usage":{"input_tokens":10,"output_tokens":5}}"#,
            ],
        );
        assert_eq!(
            out.text,
            vec!["FINAL ANSWER".to_string()],
            "the result envelope's answer becomes the output when no assistant text streamed"
        );

        // Mirror: when assistant text DID stream, the result fallback does NOT double-emit it.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(&mut adapter, CLAUDE_FIXTURE);
        assert_eq!(
            out.text,
            vec!["hello".to_string()],
            "the result fallback stays silent when assistant text already streamed (no duplicate)"
        );
    }

    #[test]
    fn t_d1_claude_result_without_usage_reports_no_usage() {
        // (M8) A `result` line with NO `usage` object must leave `usage = None` — never a fabricated
        // `Usage{0,0}` that would surface as a "$0.00, 0 tokens" CliUsage row.
        let mut adapter = ClaudeStreamJson::default();
        let out = drive(
            &mut adapter,
            &[r#"{"type":"result","subtype":"success","is_error":false,"result":"done"}"#],
        );
        assert!(
            out.usage.is_none(),
            "a result without a usage object yields no usage (no zero-token CliUsage row)"
        );
        // The answer still surfaces via the S3b fallback even with no usage.
        assert_eq!(out.text, vec!["done".to_string()]);
    }

    #[test]
    fn claude_stream_flags_not_injected_when_output_format_already_set() {
        // (M6) An operator template that already sets `--output-format json` must NOT get a second,
        // conflicting `--output-format stream-json` injected (claude would error and fail the run).
        let mut argv = build_argv("claude -p --output-format json {PROMPT}", "hi", &[]);
        let before = argv.clone();
        inject_claude_stream_flags(&mut argv);
        assert_eq!(
            argv, before,
            "no stream-json flags injected when the template already sets --output-format"
        );
    }

    #[test]
    fn t_d2_passthrough_adapter_is_one_delta_per_line_no_usage_no_files() {
        let mut adapter = Passthrough;
        let out = drive(&mut adapter, &["line one", "line two", "line three"]);
        assert_eq!(
            out.text,
            vec![
                "line one".to_string(),
                "line two".to_string(),
                "line three".to_string()
            ],
            "each raw line is exactly one delta (byte-identical to pre-adapter behavior)"
        );
        assert!(out.usage.is_none(), "passthrough never reports usage");
        assert!(out.files.is_empty(), "passthrough never reports files");
    }

    #[test]
    fn claude_binary_detection_matches_stem_only() {
        assert!(binary_is_claude("claude"));
        assert!(binary_is_claude("/usr/local/bin/claude"));
        assert!(binary_is_claude("claude.exe"));
        assert!(!binary_is_claude("agy"));
        assert!(!binary_is_claude("claude-code-wrapper"));
    }

    #[test]
    fn claude_stream_flags_inject_before_the_guard() {
        // No `--` guard (the default `claude -p {PROMPT}` shape): flags append after the prompt value —
        // the empirically-verified form (`-p <prompt> --output-format stream-json --verbose`).
        let mut argv = build_argv("claude -p {PROMPT}", "hi", &[]);
        inject_claude_stream_flags(&mut argv);
        assert_eq!(
            argv,
            vec![
                "claude",
                "-p",
                "hi",
                "--output-format",
                "stream-json",
                "--verbose"
            ]
        );
        // With a `--` guard: the flags must land BEFORE it, never demoted to positional args.
        let mut argv = build_argv("claude {PROMPT}", "hi", &[]);
        inject_claude_stream_flags(&mut argv);
        assert_eq!(
            argv,
            vec![
                "claude",
                "--output-format",
                "stream-json",
                "--verbose",
                "--",
                "hi"
            ]
        );
    }
}
