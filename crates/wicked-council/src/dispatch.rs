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

use crate::types::{
    AgenticCli, BallotContext, Category, CouncilTask, DispatchOutcome, Dispatcher, InputMode,
    SeatFailure, SeatFailureKind, Vote,
};

/// The fixed 4-question scaffold, rendered onto the task.
///
/// Options are numbered capability profiles — CLI identities are never shown to voters.
/// Voters respond with the option NUMBER, preventing self-selection bias.
pub fn render_scaffold(task: &CouncilTask) -> String {
    render_ballot(task, &BallotContext::default())
}

/// The deliberative scaffold: the base 4-question form plus the voter's seat lens, the
/// council's approval bar, and — on runoff ballots — the prior tally and dissent
/// arguments, so the council converges through visible deliberation instead of
/// re-rolling blind. `BallotContext::default()` renders the plain legacy scaffold.
pub fn render_ballot(task: &CouncilTask, ctx: &BallotContext) -> String {
    let options = task
        .options
        .iter()
        .enumerate()
        .map(|(i, o)| format!("  {}. {o}", i + 1))
        .collect::<Vec<_>>()
        .join("\n");
    let criteria = task.criteria.join(", ");

    let seat_block = match &ctx.seat {
        Some(seat) => format!(
            "You hold the **{name}** seat on the council. Evaluate primarily through this \
             lens: {lens}\n\n",
            name = seat.name,
            lens = seat.lens,
        ),
        None => String::new(),
    };

    let threshold_block = if ctx.approval_threshold > 0.0 {
        format!(
            "The council needs at least {pct}% of seats to converge on one option. If a \
             ballot fragments, a runoff round shares the tally and dissent with every seat.\n\n",
            pct = (ctx.approval_threshold * 100.0).round() as u32,
        )
    } else {
        String::new()
    };

    let runoff_block = if ctx.ballot > 1 {
        let tally = ctx
            .prior_tally
            .iter()
            .map(|(rec, n)| format!("  {n} vote(s): {rec}"))
            .collect::<Vec<_>>()
            .join("\n");
        let dissent = if ctx.dissent_arguments.is_empty() {
            String::new()
        } else {
            format!(
                "Dissenting seats argued:\n{}\n",
                ctx.dissent_arguments
                    .iter()
                    .map(|d| format!("  - {d}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        format!(
            "This is ballot {ballot} — the previous ballot did not reach the approval bar.\n\
             Prior tally:\n{tally}\n{dissent}\
             Reconsider from your seat's lens: hold your vote if your reasoning stands, or \
             converge on the strongest option if the dissent persuades you. Never converge \
             on an option you consider fundamentally unviable.\n\n",
            ballot = ctx.ballot,
        )
    } else {
        String::new()
    };

    format!(
        "You are one independent evaluator on a routing council. You do NOT know which \
         other evaluators exist or which system you are. Your only job is to pick the \
         best-fit capability profile for the task described below.\n\n\
         {seat_block}{threshold_block}{runoff_block}\
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

impl RealDispatcher {
    /// Shared dispatch body: render the given prompt, run isolated, parse the vote.
    ///
    /// Every no-vote exit names its branch. The stderr the CLI wrote is carried out with it —
    /// `run_in_isolation` has always piped stderr, and used to drop it when `output` fell out of
    /// scope, which left a non-zero exit indistinguishable from a spawn failure.
    fn dispatch_prompt(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        prompt: &str,
    ) -> DispatchOutcome {
        // Isolation: a per-dispatch tempdir under the system temp root.
        let workdir = match make_tempdir(&cli.key, &task.id) {
            Ok(d) => d,
            Err(e) => {
                return DispatchOutcome::Failed(SeatFailure::new(
                    SeatFailureKind::WorkdirUnavailable,
                    e.to_string(),
                ))
            }
        };

        let timeout = self.timeout_for(cli);
        let result = run_in_isolation(cli, prompt, &workdir, timeout);

        // Best-effort cleanup; never fail the dispatch on a cleanup error.
        let _ = std::fs::remove_dir_all(&workdir);

        let run = match result {
            Ok(r) => r,
            Err(f) => return DispatchOutcome::Failed(f),
        };
        if !run.exit_ok {
            return DispatchOutcome::Failed(
                SeatFailure {
                    kind: SeatFailureKind::NonZeroExit,
                    exit_code: run.exit_code,
                    stderr: String::new(),
                    detail: String::new(),
                }
                .with_stderr(&run.stderr),
            );
        }
        DispatchOutcome::Voted(parse_vote(cli, &run.stdout))
    }
}

impl Dispatcher for RealDispatcher {
    fn dispatch(&self, cli: &AgenticCli, task: &CouncilTask) -> Option<Vote> {
        self.dispatch_prompt(cli, task, &render_scaffold(task))
            .into_vote()
    }

    fn dispatch_ballot(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        ctx: &BallotContext,
    ) -> Option<Vote> {
        self.dispatch_ballot_detailed(cli, task, ctx).into_vote()
    }

    fn dispatch_ballot_detailed(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        ctx: &BallotContext,
    ) -> DispatchOutcome {
        self.dispatch_prompt(cli, task, &render_ballot(task, ctx))
    }
}

/// Create an isolated working directory `<tmp>/wicked-council/<task>-<cli>-<n>`.
fn make_tempdir(cli_key: &str, task_id: &str) -> std::io::Result<PathBuf> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let safe_task: String = task_id.chars().filter(|c| c.is_alphanumeric()).collect();
    let dir = std::env::temp_dir()
        .join("wicked-council")
        .join(format!("{safe_task}-{cli_key}-{n}"));
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// One isolated CLI run that reached completion.
struct IsolatedRun {
    exit_ok: bool,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
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
) -> Result<IsolatedRun, SeatFailure> {
    let mut argv = tokenize(&cli.headless_invocation);
    if argv.is_empty() {
        return Err(SeatFailure::new(
            SeatFailureKind::InvocationEmpty,
            format!(
                "`headless_invocation` for seat `{}` tokenized to nothing",
                cli.key
            ),
        ));
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
            if let Err(e) = std::fs::write(&pfile, prompt) {
                return Err(SeatFailure::new(
                    SeatFailureKind::PromptWriteFailed,
                    format!("{}: {e}", pfile.display()),
                ));
            }
            let path_str = pfile.display().to_string();
            for tok in argv.iter_mut() {
                if tok.contains("{PROMPT}") {
                    *tok = tok.replace("{PROMPT}", &path_str);
                }
            }
        }
        InputMode::PtySession => {
            // The council dispatcher doesn't manage PTY sessions; skip this seat entirely.
            // No shipped seat declares this mode, but one that did would be structurally
            // incapable of ever voting — worth naming rather than looking like a timeout.
            return Err(SeatFailure::new(
                SeatFailureKind::PtyUnsupported,
                format!(
                    "seat `{}` declares InputMode::PtySession, which the council dispatcher \
                     does not manage",
                    cli.key
                ),
            ));
        }
    }

    // Append trust flags so the CLI never blocks on an interactive prompt.
    argv.extend(cli.trust_flags.iter().cloned());

    let Some((program, args)) = argv.split_first() else {
        // Unreachable while the `argv.is_empty()` guard above stands, but splitting after the
        // trust-flag extend means the guard is no longer adjacent to this call.
        return Err(SeatFailure::new(
            SeatFailureKind::InvocationEmpty,
            format!("no program token in argv for seat `{}`", cli.key),
        ));
    };

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

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return Err(SeatFailure::new(
                SeatFailureKind::SpawnFailed,
                format!("{program}: {e}"),
            ))
        }
    };

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
                    // Reap, and keep whatever the CLI wrote before the budget ran out. A partial
                    // stderr is frequently the whole diagnosis — an auth prompt, a rate-limit
                    // notice — and the old code discarded it along with the exit status.
                    let partial = child
                        .wait_with_output()
                        .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
                        .unwrap_or_default();
                    return Err(SeatFailure::new(
                        SeatFailureKind::TimedOut,
                        format!("exceeded {timeout:?} dispatch budget"),
                    )
                    .with_stderr(&partial));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(SeatFailure::new(SeatFailureKind::WaitFailed, e.to_string())),
        }
    }

    let output = match child.wait_with_output() {
        Ok(o) => o,
        Err(e) => return Err(SeatFailure::new(SeatFailureKind::WaitFailed, e.to_string())),
    };
    Ok(IsolatedRun {
        exit_ok: output.status.success(),
        exit_code: output.status.code(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::SEATS;

    fn task() -> CouncilTask {
        CouncilTask {
            id: "t".into(),
            topic: "route this".into(),
            options: vec!["profile one".into(), "profile two".into()],
            criteria: vec!["general".into()],
            session_id: "s".into(),
        }
    }

    #[test]
    fn plain_scaffold_has_no_seat_threshold_or_runoff_blocks() {
        let p = render_scaffold(&task());
        assert!(!p.contains("seat on the council"));
        assert!(!p.contains("approval"));
        assert!(!p.contains("Prior tally"));
    }

    #[test]
    fn first_ballot_renders_seat_and_threshold() {
        let ctx = BallotContext {
            seat: Some(SEATS[1].clone()),
            ballot: 1,
            approval_threshold: 0.75,
            prior_tally: vec![],
            dissent_arguments: vec![],
        };
        let p = render_ballot(&task(), &ctx);
        assert!(p.contains("Risk & Failure Modes"), "seat name rendered");
        assert!(p.contains("75%"), "approval bar stated");
        assert!(!p.contains("Prior tally"), "no runoff block on ballot 1");
    }

    #[test]
    fn runoff_ballot_renders_tally_and_dissent() {
        let ctx = BallotContext {
            seat: Some(SEATS[0].clone()),
            ballot: 2,
            approval_threshold: 0.75,
            prior_tally: vec![("1 — fits".into(), 3), ("2 — other".into(), 1)],
            dissent_arguments: vec!["profile two lacks repo context".into()],
        };
        let p = render_ballot(&task(), &ctx);
        assert!(p.contains("ballot 2"));
        assert!(p.contains("3 vote(s): 1 — fits"));
        assert!(p.contains("profile two lacks repo context"));
        assert!(
            p.contains("Never converge"),
            "anti-groupthink guard present"
        );
    }
}

/// FINDING-026 fix #1: every no-vote names its branch and keeps the CLI's own words.
///
/// These drive the REAL dispatcher against real processes, because the defect was never in the
/// logic — it was that `Option<Vote>` had nowhere to put the answer. A stub dispatcher would
/// assert nothing about it.
///
/// Cross-platform per the pattern already used in `probe.rs`: `sh` on unix, `cmd` on Windows.
#[cfg(test)]
mod failure_diagnostics_tests {
    use super::*;
    use crate::types::{Category, Confidence, STDERR_CAPTURE_LIMIT};

    fn task() -> CouncilTask {
        CouncilTask {
            id: "t".into(),
            topic: "route this".into(),
            options: vec!["profile one".into()],
            criteria: vec!["general".into()],
            session_id: "s".into(),
        }
    }

    /// A seat whose `headless_invocation` is the given argv template. No `{PROMPT}` — these
    /// tests are about the failure path, not prompt delivery.
    fn seat(key: &str, binary: &str, invocation: &str) -> AgenticCli {
        AgenticCli {
            key: key.to_string(),
            display_name: key.to_string(),
            binary: binary.to_string(),
            headless_invocation: invocation.to_string(),
            category: Category::AgenticCoder,
            input_mode: InputMode::PromptArg,
            version_probe: vec![],
            trust_flags: vec![],
            alt_binaries: vec![],
            confidence: Confidence::Verified,
            enabled_for_council: true,
            acp: None,
            capabilities: None,
        }
    }

    /// A shell seat running `script`, spelled for the host platform.
    fn shell_seat(key: &str, script: &str) -> AgenticCli {
        if cfg!(windows) {
            seat(key, "cmd", &format!("cmd /C \"{script}\""))
        } else {
            seat(key, "sh", &format!("sh -c \"{script}\""))
        }
    }

    fn failure_of(cli: &AgenticCli, timeout: Duration) -> SeatFailure {
        let d = RealDispatcher {
            timeout,
            local_runner_timeout: timeout,
        };
        match d.dispatch_prompt(cli, &task(), "ignored") {
            DispatchOutcome::Failed(f) => f,
            DispatchOutcome::Voted(_) => {
                panic!("this seat must not vote — the test is about the failure path")
            }
        }
    }

    #[test]
    fn a_missing_binary_reports_spawn_failed_and_names_it() {
        let cli = seat(
            "ghost",
            "wicked-council-no-such-binary-xyzzy",
            "wicked-council-no-such-binary-xyzzy --headless",
        );
        let f = failure_of(&cli, Duration::from_secs(5));
        assert_eq!(f.kind, SeatFailureKind::SpawnFailed);
        // Naming the binary is the difference between "council did not reach a vote" and a
        // one-line fix.
        assert!(
            f.detail.contains("wicked-council-no-such-binary-xyzzy"),
            "the detail must name the binary that could not spawn: {f:?}"
        );
    }

    #[test]
    fn a_non_zero_exit_keeps_the_code_and_the_stderr() {
        // The leading hypothesis for the observed 92.6% degradation was a non-zero exit whose
        // stderr was discarded. This is the branch that used to throw the evidence away.
        let cli = shell_seat("boom", "echo diagnostic-needle 1>&2; exit 3");
        let f = failure_of(&cli, Duration::from_secs(30));
        assert_eq!(f.kind, SeatFailureKind::NonZeroExit);
        assert_eq!(f.exit_code, Some(3), "the exit code must survive: {f:?}");
        assert!(
            f.stderr.contains("diagnostic-needle"),
            "the CLI's own stderr is the artifact that identifies the failure: {f:?}"
        );
        // And it has to reach the one-line rendering the degrade string uses.
        let reason = f.reason();
        assert!(reason.contains("non_zero_exit"), "{reason}");
        assert!(reason.contains("exit 3"), "{reason}");
        assert!(reason.contains("diagnostic-needle"), "{reason}");
    }

    #[test]
    fn a_seat_that_outlives_the_budget_reports_timed_out_not_a_bare_no_vote() {
        // 250ms budget against a process that sleeps far longer.
        let cli = if cfg!(windows) {
            seat("slow", "cmd", "cmd /C \"ping -n 30 127.0.0.1\"")
        } else {
            shell_seat("slow", "sleep 30")
        };
        let f = failure_of(&cli, Duration::from_millis(250));
        assert_eq!(f.kind, SeatFailureKind::TimedOut);
        assert!(
            f.detail.contains("budget"),
            "the timeout must say it was a budget, not an error: {f:?}"
        );
        // A sub-second budget must render as itself. `as_secs()` truncated this to
        // "exceeded 0s dispatch budget", which reads as a bug in the budget rather than a
        // slow seat — the exact confusion this finding is about.
        assert!(
            f.detail.contains("250ms"),
            "a sub-second budget must not be truncated to 0s: {f:?}"
        );
    }

    #[test]
    fn a_pty_seat_is_named_rather_than_silently_skipped() {
        // `InputMode::PtySession` returns before spawning anything. No shipped seat declares it,
        // but one that did would never vote — and used to look exactly like a timeout.
        let mut cli = shell_seat("pty", "exit 0");
        cli.input_mode = InputMode::PtySession;
        let f = failure_of(&cli, Duration::from_secs(5));
        assert_eq!(f.kind, SeatFailureKind::PtyUnsupported);
        assert!(f.detail.contains("PtySession"), "{f:?}");
    }

    #[test]
    fn an_empty_invocation_is_named() {
        let cli = seat("empty", "irrelevant", "   ");
        let f = failure_of(&cli, Duration::from_secs(5));
        assert_eq!(f.kind, SeatFailureKind::InvocationEmpty);
    }

    #[test]
    fn a_seat_that_exits_zero_votes() {
        let cli = shell_seat("ok", "echo RECOMMENDATION: 1");
        let d = RealDispatcher {
            timeout: Duration::from_secs(30),
            local_runner_timeout: Duration::from_secs(30),
        };
        let outcome = d.dispatch_prompt(&cli, &task(), "ignored");
        // The control case: without it, a bug that made EVERY dispatch fail would still pass
        // every assertion above.
        assert!(outcome.is_voted(), "exit 0 must produce a vote");
    }

    #[test]
    fn captured_stderr_is_bounded_and_stays_valid_utf8() {
        // A runaway CLI must not balloon an event payload. The truncation walks back to a char
        // boundary, so a multi-byte char straddling the limit cannot corrupt the string.
        let huge = "é".repeat(STDERR_CAPTURE_LIMIT);
        let f = SeatFailure::new(SeatFailureKind::NonZeroExit, "").with_stderr(&huge);
        assert!(f.stderr.len() <= STDERR_CAPTURE_LIMIT);
        assert!(f.stderr.chars().all(|c| c == 'é'));
    }

    #[test]
    fn the_reason_line_never_spans_lines() {
        // Degrade reasons land in single-line contexts (events, the studio's routing badge).
        let f = SeatFailure::new(SeatFailureKind::SpawnFailed, "").with_stderr("one\ntwo\nthree");
        let reason = f.reason();
        assert!(!reason.contains('\n'), "{reason}");
        assert!(reason.contains("one two three"), "{reason}");
    }
}
