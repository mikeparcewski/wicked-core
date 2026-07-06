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

use crate::workflow::{DeltaSink, StepInput, StepOutput, StepRunner, StepStatus};

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
        let argv = build_argv(&invocation, &input.unit.description);

        // Run in the worktree if the run targets a repo; else a per-run temp sandbox (never the
        // orchestrator's own cwd).
        let cwd = input.workdir.clone().unwrap_or_else(|| sandbox_for(input));
        let _ = std::fs::create_dir_all(&cwd);

        let (status, output) = if argv.is_empty() {
            (
                StepStatus::Failed,
                format!("(no invocation configured for cli `{cli_key}`)"),
            )
        } else {
            let mut cmd = Command::new(&argv[0]);
            cmd.args(&argv[1..]).current_dir(&cwd);
            match run_bounded(cmd, self.timeout, emit) {
                Ok((0, out, _)) => (StepStatus::Ok, out),
                Ok((-1, _, err)) if err == TIMED_OUT => (
                    StepStatus::Cancelled,
                    format!("(cli `{cli_key}` exceeded the timeout and was killed)"),
                ),
                Ok((code, out, err)) => {
                    let detail = if !out.trim().is_empty() { out } else { err };
                    (
                        StepStatus::Failed,
                        format!("(cli `{cli_key}` exited {code}) {detail}"),
                    )
                }
                Err(e) => (
                    StepStatus::Failed,
                    format!("(could not run `{}`: {e})", argv[0]),
                ),
            }
        };

        StepOutput {
            run_id: input.run_id.clone(),
            unit_ix: input.unit_ix,
            attempt: input.attempt,
            output,
            status,
        }
    }
}

/// A per-run temp sandbox for repo-less runs (so a real CLI never edits the orchestrator's own tree).
fn sandbox_for(input: &StepInput) -> PathBuf {
    std::env::temp_dir()
        .join("wicked-core-sandbox")
        .join(&input.run_id)
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

/// Run `cmd` bounded by `timeout`, draining stdout+stderr CONCURRENTLY (no pipe-buffer deadlock) and
/// STREAMING each stdout line through `emit` as it arrives (live output). Returns `(exit_code, stdout,
/// stderr)`; a timeout returns `(-1, "", TIMED_OUT)` after killing. Uses a scoped thread so the stdout
/// drain can borrow `emit` (which lives on the worker stack).
fn run_bounded(
    mut cmd: Command,
    timeout: Duration,
    emit: &DeltaSink,
) -> std::io::Result<(i32, String, String)> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn()?;
    let so = child.stdout.take().expect("piped stdout");
    let se = child.stderr.take().expect("piped stderr");
    let child_ref = &mut child;

    // Cap the ACCUMULATED buffers so a runaway/verbose CLI can't OOM the orchestrator. Streaming via
    // `emit` is unaffected (every line still streams); only the retained string is bounded.
    const MAX_OUT: usize = 8 * 1024 * 1024;

    let (code, timed_out, out, err) = std::thread::scope(|scope| {
        // Stdout: read line-by-line, stream each line through `emit`, accumulate (bounded).
        let out_h = scope.spawn(move || {
            use std::io::BufRead;
            let mut s = String::new();
            let mut capped = false;
            for line in std::io::BufReader::new(so).lines().map_while(Result::ok) {
                emit(&line);
                if s.len() < MAX_OUT {
                    s.push_str(&line);
                    s.push('\n');
                } else if !capped {
                    s.push_str("\n… (output truncated)\n");
                    capped = true;
                }
            }
            s
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
        let out = out_h.join().unwrap_or_default();
        let err = err_h.join().unwrap_or_default();
        (code, timed_out, out, err)
    });

    if timed_out {
        // Preserve what the CLI produced before the kill (debugging context on a hang).
        Ok((-1, out, TIMED_OUT.to_string()))
    } else {
        Ok((code, out, err))
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
pub(crate) fn build_argv(invocation: &str, prompt: &str) -> Vec<String> {
    let toks = tokenize(invocation);
    let mut argv: Vec<String> = Vec::new();
    let mut placed = false;
    let ensure_guard = |argv: &mut Vec<String>| {
        let prev_is_flag = argv.last().map(|p| p.starts_with('-')).unwrap_or(false);
        if !prev_is_flag && !argv.iter().any(|a| a == "--") {
            argv.push("--".to_string());
        }
    };
    for t in &toks {
        if t == "{PROMPT}" {
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
        let (code, out, _err) = run_bounded(cmd, Duration::from_secs(5), &emit).unwrap();
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
    fn prompt_is_a_guarded_standalone_arg() {
        assert_eq!(
            build_argv("echo {PROMPT}", "--help"),
            vec!["echo".to_string(), "--".to_string(), "--help".to_string()]
        );
    }

    #[test]
    fn flag_value_position_keeps_binding() {
        assert_eq!(
            build_argv("claude -p {PROMPT}", "hi"),
            vec!["claude".to_string(), "-p".to_string(), "hi".to_string()]
        );
    }

    #[test]
    fn unknown_cli_falls_back_to_key_as_binary() {
        // A key not in the registry becomes `<key> {PROMPT}`.
        let inv = resolve_invocation("definitely-not-a-registered-cli-xyz");
        assert_eq!(inv, "definitely-not-a-registered-cli-xyz {PROMPT}");
    }
}
