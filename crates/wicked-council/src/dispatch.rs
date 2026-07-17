//! Isolated, timeboxed dispatch of the scaffold to one CLI, and vote parsing.
//!
//! Isolation (non-negotiable): each CLI runs in its **own tempdir**, under a **per-CLI
//! timeout**, with **stdin from null**, and its `trust_flags` appended so it never blocks
//! on a permission/trust prompt. No CLI sees another's output.
//!
//! The scaffold's 4 questions are rendered into the prompt; the CLI is expected to answer
//! with `KEY: value` lines we parse back into a [`Vote`]. Real LLM CLIs are coached by the
//! prompt to use this format; the E2E test uses fake-CLI shell scripts that echo exactly
//! these lines.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::types::{AgenticCli, Category, CouncilTask, Dispatcher, InputMode, Vote};

/// The fixed 4-question scaffold, rendered onto the task.
///
/// Options are numbered capability profiles — CLI identities are never shown to voters.
/// Voters respond with the option NUMBER, preventing self-selection bias.
pub fn render_scaffold(task: &CouncilTask) -> String {
    let options = task
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("  {}. {o}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let criteria = task.criteria.join(", ");
    format!(
        "You are one independent evaluator on a routing council. You do NOT know which \
         other evaluators exist or which system you are. Your only job is to pick the \
         best-fit capability profile for the task described below.\n\n\
         Task: {topic}\n\n\
         Capability profiles:\n{options}\n\n\
         Evaluation criteria: {criteria}\n\n\
         Answer with EXACTLY these four lines. For RECOMMENDATION, give the option NUMBER \
         only (e.g. \"2\"), followed by a brief rationale — do NOT name any tool, CLI, or \
         AI system:\n\
         RECOMMENDATION: <option number and rationale>\n\
         TOP_RISK: <the single biggest risk with that profile for this task>\n\
         CHANGE_MY_MIND: <evidence or condition that would reverse your pick>\n\
         DISQUALIFIER: <option number of any profile fundamentally unviable for this task, or 'None'>",
        topic = task.topic,
    )
}

/// The real, subprocess-backed dispatcher.
#[derive(Debug, Clone)]
pub struct RealDispatcher {
    /// Timeout for agentic/chat CLIs.
    pub timeout: Duration,
    /// Longer timeout for local runners (cold model load).
    pub local_runner_timeout: Duration,
}

impl Default for RealDispatcher {
    fn default() -> Self {
        RealDispatcher {
            timeout: Duration::from_secs(60),
            local_runner_timeout: Duration::from_secs(120),
        }
    }
}

impl RealDispatcher {
    fn timeout_for(&self, cli: &AgenticCli) -> Duration {
        match cli.category {
            Category::LocalRunner => self.local_runner_timeout,
            _ => self.timeout,
        }
    }
}

impl Dispatcher for RealDispatcher {
    fn dispatch(&self, cli: &AgenticCli, task: &CouncilTask) -> Option<Vote> {
        let prompt = render_scaffold(task);

        // Isolation: a per-dispatch tempdir under the system temp root.
        let workdir = make_tempdir(&cli.key, &task.id)?;

        let timeout = self.timeout_for(cli);
        let result = run_in_isolation(cli, &prompt, &workdir, timeout);

        // Best-effort cleanup; never fail the dispatch on a cleanup error.
        let _ = std::fs::remove_dir_all(&workdir);

        let (exit_ok, stdout) = result?;
        if !exit_ok {
            return None;
        }
        Some(parse_vote(cli, &stdout))
    }
}

/// Create an isolated working directory `<tmp>/wicked-council/<task>-<cli>-<n>`.
fn make_tempdir(cli_key: &str, task_id: &str) -> Option<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let safe_task: String = task_id.chars().filter(|c| c.is_alphanumeric()).collect();
    let dir = std::env::temp_dir()
        .join("wicked-council")
        .join(format!("{safe_task}-{cli_key}-{n}"));
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Build the argv from `headless_invocation` and `input_mode`, run it isolated in
/// `workdir` bounded by `timeout`, and return `(exit_success, stdout)`.
///
/// We do **not** invoke a shell — we split the template into argv ourselves (simple
/// whitespace tokenizer that respects double-quotes), substitute `{PROMPT}` per the input
/// mode, and append `trust_flags`. Avoiding a shell sidesteps the quoting foot-guns
/// apostrophes in topics would otherwise cause.
fn run_in_isolation(
    cli: &AgenticCli,
    prompt: &str,
    workdir: &PathBuf,
    timeout: Duration,
) -> Option<(bool, String)> {
    let mut argv = tokenize(&cli.headless_invocation);
    if argv.is_empty() {
        return None;
    }

    // Substitute / deliver the prompt per input mode.
    let mut stdin_payload: Option<String> = None;
    match cli.input_mode {
        InputMode::PromptArg => {
            for tok in argv.iter_mut() {
                if tok.contains("{PROMPT}") {
                    *tok = tok.replace("{PROMPT}", prompt);
                }
            }
        }
        InputMode::Stdin => {
            // Drop any {PROMPT} placeholder from argv; the prompt goes on stdin.
            for tok in argv.iter_mut() {
                if tok.contains("{PROMPT}") {
                    *tok = tok.replace("{PROMPT}", "");
                }
            }
            stdin_payload = Some(prompt.to_string());
        }
        InputMode::AtFile | InputMode::MessageFile => {
            // Write the prompt to a file inside the isolated workdir, substitute path.
            let pfile = workdir.join("prompt.txt");
            std::fs::write(&pfile, prompt).ok()?;
            let path_str = pfile.display().to_string();
            for tok in argv.iter_mut() {
                if tok.contains("{PROMPT}") {
                    *tok = tok.replace("{PROMPT}", &path_str);
                }
            }
        }
        InputMode::PtySession => {
            // The council dispatcher doesn't manage PTY sessions; skip this seat entirely.
            return None;
        }
    }

    // Append trust flags so the CLI never blocks on an interactive prompt.
    argv.extend(cli.trust_flags.iter().cloned());

    let (program, args) = argv.split_first()?;

    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if stdin_payload.is_some() {
        command.stdin(Stdio::piped());
    } else {
        command.stdin(Stdio::null());
    }

    let mut child = command.spawn().ok()?;

    if let Some(payload) = stdin_payload {
        use std::io::Write;
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(payload.as_bytes());
            // Drop closes stdin so the child sees EOF.
        }
    }

    // Bounded wait (watcher loop, std-only).
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None; // treated as timeout → no vote
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }

    let output = child.wait_with_output().ok()?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    Some((output.status.success(), stdout))
}

/// Whitespace tokenizer that keeps double-quoted spans together and strips the
/// surrounding quotes. Good enough for the registry templates (no nested quoting).
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

/// Parse the CLI's stdout into a [`Vote`]. Tolerant: matches `KEY:` prefixes
/// case-insensitively, accepts `TOP_RISK`/`TOP RISK`, and falls back to empty strings for
/// missing fields (the synthesis layer treats empty risks as "no risk cited", which simply
/// doesn't converge).
pub fn parse_vote(cli: &AgenticCli, stdout: &str) -> Vote {
    let mut recommendation = String::new();
    let mut top_risk = String::new();
    let mut change_my_mind = String::new();
    let mut disqualifier_raw = String::new();

    for line in stdout.lines() {
        let trimmed = line.trim();
        if let Some(v) = strip_key(trimmed, "RECOMMENDATION") {
            recommendation = v;
        } else if let Some(v) =
            strip_key(trimmed, "TOP_RISK").or_else(|| strip_key(trimmed, "TOP RISK"))
        {
            top_risk = v;
        } else if let Some(v) =
            strip_key(trimmed, "CHANGE_MY_MIND").or_else(|| strip_key(trimmed, "CHANGE MY MIND"))
        {
            change_my_mind = v;
        } else if let Some(v) = strip_key(trimmed, "DISQUALIFIER") {
            disqualifier_raw = v;
        }
    }

    let disqualifier = match disqualifier_raw.trim() {
        "" => None,
        s if s.eq_ignore_ascii_case("none") => None,
        s => Some(s.to_string()),
    };

    Vote {
        cli: cli.key.clone(),
        recommendation,
        top_risk,
        change_my_mind,
        disqualifier,
        // The vote carries the record's confidence label; never averaged.
        confidence: cli.confidence,
        provenance: format!(
            "cli={} ({}), isolated tempdir, stdin=null",
            cli.key, cli.display_name
        ),
    }
}

/// If `line` starts with `KEY:` (case-insensitive), return the trimmed remainder.
fn strip_key(line: &str, key: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    let needle = format!("{}:", key.to_ascii_lowercase());
    if lower.starts_with(&needle) {
        Some(line[needle.len()..].trim().to_string())
    } else {
        None
    }
}
