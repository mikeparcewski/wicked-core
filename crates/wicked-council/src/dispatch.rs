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
use wicked_apps_core::HardenedCommand;

use crate::types::{
    AgenticCli, BallotContext, Category, CouncilTask, DispatchOutcome, Dispatcher, InputMode,
    SeatFailure, SeatFailureKind, TimedOutcome, Vote, STDERR_CAPTURE_LIMIT,
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

/// Env var overriding the agentic/chat CLI budget, in whole seconds.
pub const ENV_TIMEOUT_SECS: &str = "WICKED_COUNCIL_TIMEOUT_SECS";
/// Env var overriding the local-runner budget, in whole seconds.
pub const ENV_LOCAL_TIMEOUT_SECS: &str = "WICKED_COUNCIL_LOCAL_TIMEOUT_SECS";

/// Default budget for an agentic/chat CLI seat.
///
/// A council ballot asks a coding CLI to read a task, weigh several capability profiles and
/// justify a pick — a reasoning turn, not a shell command. Measured cost of that turn on the
/// shipped roster is 21.5–35.5 s (`tools/council_probe.py`, 8 ballots), so anything near 30 s
/// kills seats that were about to answer and reports it as the seat's failure.
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
/// Default budget for a local runner, which pays a cold model load before it reasons at all.
const DEFAULT_LOCAL_RUNNER_TIMEOUT: Duration = Duration::from_secs(120);

impl Default for RealDispatcher {
    fn default() -> Self {
        RealDispatcher {
            timeout: DEFAULT_TIMEOUT,
            local_runner_timeout: DEFAULT_LOCAL_RUNNER_TIMEOUT,
        }
    }
}

/// Parse a whole-second duration from a raw env value, falling back when it says nothing usable.
///
/// A malformed or zero value falls back rather than failing the run: the council is a routing
/// aid, and refusing to dispatch because an env var is misspelled trades a degraded decision for
/// no decision. Zero is rejected specifically because it would kill every seat the instant it
/// spawned and report the result as a roster-wide timeout — the same symptom this finding is
/// about, reintroduced by configuration.
///
/// Takes the value rather than the variable name so it is testable without mutating the
/// process environment, which test threads share.
fn secs_or(raw: Option<String>, fallback: Duration) -> Duration {
    raw.and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|&n| n > 0)
        .map(Duration::from_secs)
        .unwrap_or(fallback)
}

impl RealDispatcher {
    /// The dispatcher the engine runs, with both budgets overridable from the environment.
    ///
    /// Callers should prefer this over constructing the struct literally. The engine used to
    /// hardcode 30 s for both fields — half the default below, and short enough that the shipped
    /// roster timed out on most ballots (FINDING-026). Routing the value through one function
    /// keeps the production budget and the documented default from drifting apart again.
    pub fn from_env() -> Self {
        RealDispatcher {
            timeout: secs_or(std::env::var(ENV_TIMEOUT_SECS).ok(), DEFAULT_TIMEOUT),
            local_runner_timeout: secs_or(
                std::env::var(ENV_LOCAL_TIMEOUT_SECS).ok(),
                DEFAULT_LOCAL_RUNNER_TIMEOUT,
            ),
        }
    }

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
        self.dispatch_prompt_timed(cli, task, prompt).outcome
    }

    /// The dispatch path, reporting the queue wait and the run separately. See
    /// [`TimedOutcome`] for why the two must not be summed.
    fn dispatch_prompt_timed(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        prompt: &str,
    ) -> TimedOutcome {
        // Isolation: a per-dispatch tempdir under the system temp root.
        let workdir = match make_tempdir(&cli.key, &task.id) {
            Ok(d) => d,
            Err(e) => {
                return TimedOutcome {
                    outcome: DispatchOutcome::Failed(SeatFailure::new(
                        SeatFailureKind::WorkdirUnavailable,
                        e.to_string(),
                    )),
                    queued_ms: 0,
                    ran_ms: 0,
                }
            }
        };

        let timeout = self.timeout_for(cli);
        // Hold a permit for the subprocess only. Seats are dispatched concurrently at two levels
        // — every unit convenes its own council, and every council now dispatches its own seats —
        // and those multiply. Without a ceiling, a 3-unit run on a 3-seat roster puts 9 agentic
        // CLIs on the machine at once and they starve each other into their own budgets.
        //
        // The permit is taken here, not around the whole ballot, so a seat waiting for one is not
        // burning its budget: `run_in_isolation` starts the clock when it spawns.
        let queue_started = Instant::now();
        let (result, queued, ran) = {
            let _permit = seat_permits().acquire();
            let queued = queue_started.elapsed();
            let run_started = Instant::now();
            let result = run_in_isolation(cli, prompt, &workdir, timeout);
            (result, queued, run_started.elapsed())
        };

        // Best-effort cleanup; never fail the dispatch on a cleanup error.
        let _ = std::fs::remove_dir_all(&workdir);

        let outcome = match result {
            Err(f) => DispatchOutcome::Failed(f),
            Ok(run) if !run.exit_ok => DispatchOutcome::Failed(
                SeatFailure {
                    kind: SeatFailureKind::NonZeroExit,
                    exit_code: run.exit_code,
                    stderr: String::new(),
                    detail: String::new(),
                }
                .with_stderr(&run.stderr),
            ),
            Ok(run) => DispatchOutcome::Voted(parse_vote(cli, &run.stdout)),
        };
        TimedOutcome {
            outcome,
            queued_ms: queued.as_millis() as u64,
            ran_ms: ran.as_millis() as u64,
        }
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

    fn dispatch_ballot_timed(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        ctx: &BallotContext,
    ) -> TimedOutcome {
        self.dispatch_prompt_timed(cli, task, &render_ballot(task, ctx))
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

    // A seat is a real agentic CLI with tool access, running in the caller's workdir — the same
    // shape of process as a governed worker, reached by a different path. FINDING-067 hardened the
    // worker paths; enumerating `Command::new` afterwards found this one had never been considered,
    // and it inherited the daemon's entire environment. Latent rather than live (nothing sets
    // `WICKED_ESTATE_DB` in the daemon today) — but "latent" here means "until an operator who works
    // on estate exports it in the shell that starts the daemon".
    let mut command = Command::new(program);
    command
        .hardened()
        .args(args)
        .current_dir(workdir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    // Give the seat its own process group so the timeout path can signal the whole tree. Without
    // this, killing a CLI that shelled out leaves the grandchild alive and holding our pipes.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }

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
                    // Kill the whole group, not just the seat's own pid: an agentic CLI that
                    // shelled out leaves the grandchild holding the pipes we are about to read.
                    kill_process_tree(&mut child);
                    // Keep whatever the CLI wrote before the budget ran out — a partial stderr is
                    // frequently the whole diagnosis (an auth prompt, a rate-limit notice). The
                    // drain is itself bounded; see `drain_stderr`.
                    let partial = drain_stderr(&mut child, DRAIN_BUDGET);
                    // Reap. `wait` does not read the pipes, so unlike `wait_with_output` it
                    // returns as soon as the direct child is collected.
                    let _ = child.wait();
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

/// Env var overriding how many seat subprocesses may run at once, process-wide.
pub const ENV_MAX_CONCURRENT_SEATS: &str = "WICKED_COUNCIL_MAX_CONCURRENT_SEATS";

/// Default ceiling on concurrent seat subprocesses.
///
/// Not a CPU bound — a seat is a network-bound LLM client that spends its budget waiting, and the
/// host has cores to spare. The bound that bites is the shared one: every extra concurrent seat
/// competes for the same provider quota and the same few hundred MB apiece, and a seat starved
/// past its budget is indistinguishable from a broken one.
///
/// Three is what the sequential build effectively ran (one seat per council, three councils) and
/// the configuration under which the roster demonstrably voted.
const DEFAULT_MAX_CONCURRENT_SEATS: usize = 3;

/// A counting semaphore over concurrent seat subprocesses, shared by every council in the process.
struct SeatPermits {
    /// Permits still available.
    free: std::sync::Mutex<usize>,
    /// Signalled when a permit is returned.
    returned: std::sync::Condvar,
}

impl SeatPermits {
    /// Block until a permit is available, then hold it until the guard drops.
    fn acquire(&self) -> SeatPermit<'_> {
        let mut free = self.free.lock().unwrap_or_else(|e| e.into_inner());
        while *free == 0 {
            free = self.returned.wait(free).unwrap_or_else(|e| e.into_inner());
        }
        *free -= 1;
        SeatPermit { permits: self }
    }
}

/// Returns its permit on drop, including when the dispatch unwinds.
struct SeatPermit<'a> {
    permits: &'a SeatPermits,
}

impl Drop for SeatPermit<'_> {
    fn drop(&mut self) {
        let mut free = self.permits.free.lock().unwrap_or_else(|e| e.into_inner());
        *free += 1;
        self.permits.returned.notify_one();
    }
}

/// The process-wide seat permits, sized from the environment on first use.
fn seat_permits() -> &'static SeatPermits {
    static PERMITS: std::sync::OnceLock<SeatPermits> = std::sync::OnceLock::new();
    PERMITS.get_or_init(|| SeatPermits {
        free: std::sync::Mutex::new(max_concurrent_seats(
            std::env::var(ENV_MAX_CONCURRENT_SEATS).ok(),
        )),
        returned: std::sync::Condvar::new(),
    })
}

/// Parse the seat ceiling from a raw env value.
///
/// Anything unusable falls back to the default. Zero is rejected for a specific reason: a ceiling
/// of zero is not "no limit", it is a deadlock — every seat would block forever waiting for a
/// permit that no one holds.
fn max_concurrent_seats(raw: Option<String>) -> usize {
    raw.and_then(|raw| raw.trim().parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or(DEFAULT_MAX_CONCURRENT_SEATS)
}

/// How long the timeout path will wait for a killed seat's stderr to drain.
///
/// Bounding this is what makes the dispatch budget mean anything. `wait_with_output` reads the
/// pipes to EOF, and EOF does not arrive while *any* process still holds the write end — so a
/// grandchild that outlived its parent used to extend a 30 s budget to 72 s (measured), with no
/// upper bound in principle. Two seconds is far more than a dead process needs to flush what it
/// already wrote, and the only thing a longer value could buy is a longer overrun.
const DRAIN_BUDGET: Duration = Duration::from_secs(2);

/// SIGKILL the seat's whole process group, falling back to the direct child.
///
/// Unix only signals the group — `process_group(0)` at spawn made the child a group leader, so
/// `kill -KILL -<pid>` reaches everything it started. We shell out to `kill(1)` rather than take a
/// `libc` dependency for one call; a failure here is not worth reporting because the direct-child
/// kill below is the outcome that actually matters, and the drain is bounded either way.
///
/// On Windows there is no group to signal, so this is exactly the old behaviour: the direct child
/// dies, any grandchild is left to the bounded drain.
/// Minimal direct FFI into libc (always linked on unix) so we can signal the child's whole PROCESS
/// GROUP. Declared here rather than taking a `libc` crate dependency, matching the pattern already
/// used in `wicked-core`'s terminal teardown.
#[cfg(unix)]
mod sig {
    extern "C" {
        pub fn killpg(pgrp: i32, sig: i32) -> i32;
        pub fn getpgid(pid: i32) -> i32;
    }
    pub const SIGKILL: i32 = 9;
}

/// Kill the seat and everything it spawned.
///
/// Killing only the direct child is what let a timed-out seat overrun its budget: a grandchild
/// that outlives it holds the pipes open. The seat is spawned into its own process group
/// (`process_group(0)`), so on unix one `killpg` reaches the whole tree.
///
/// `killpg` rather than shelling out to `kill(1)`: no `PATH` lookup to hijack, no dependency on a
/// userland binary that a minimal container may not ship, and no argument parsing to get wrong.
/// SIGKILL directly rather than a SIGTERM grace — the seat has already exceeded its budget, and
/// the whole point of this path is that it stops costing time.
fn kill_process_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // Signal the group ONLY if this child actually leads one.
        //
        // `run_in_isolation` spawns every seat with `process_group(0)`, so its pid IS the group
        // id. A child spawned without that flag inherits OUR group instead — and passing its pid
        // to `killpg` would then SIGKILL our own process group, this process included. Checked
        // rather than assumed: the cost of the check is one syscall and the cost of being wrong
        // is the whole daemon. (Found the hard way: an early version of the drain test spawned
        // without the flag and killed the test runner.)
        if unsafe { sig::getpgid(pid) } == pid {
            unsafe { sig::killpg(pid, sig::SIGKILL) };
        }
    }
    #[cfg(windows)]
    {
        // No process groups to signal here, and `Child::kill` reaches only the direct child.
        // `taskkill /T` walks the tree by parent pid, which is the closest equivalent.
        let _ = Command::new("taskkill")
            .hardened()
            .args(["/T", "/F", "/PID"])
            .arg(child.id().to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // Belt and braces on every platform: if the group/tree kill did not apply or did not take,
    // the direct child still dies here.
    let _ = child.kill();
}

/// Read what the seat wrote to stderr, giving up after `budget`.
///
/// The read happens on its own thread because there is no portable way to poll a pipe for
/// readiness in std. If the budget expires the thread is abandoned rather than joined: it is
/// blocked on a pipe whose writer we do not control, and joining it would reintroduce the exact
/// unbounded wait this function exists to remove. It exits on its own once the last writer closes.
fn drain_stderr(child: &mut std::process::Child, budget: Duration) -> String {
    let Some(mut pipe) = child.stderr.take() else {
        return String::new();
    };
    // The reader appends into a SHARED buffer rather than sending its result at the end, because
    // the end may never come. Sending once — at EOF or at the cap — hands the caller an empty
    // string in precisely the case the diagnostic matters most: a seat that is still writing when
    // the budget expires. The bytes existed, the thread was holding them, and the caller got
    // nothing. Appending under a lock means the budget bounds how long we WAIT, not how much of
    // what was already read we are allowed to keep.
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let writer = std::sync::Arc::clone(&buf);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::Read;
        let mut chunk = [0u8; 512];
        let mut total = 0usize;
        // Bounded at the READ, not just at the wait. The wait below is what this function
        // returns on; this thread outlives it and keeps reading. A grandchild that survived the
        // group kill and is writing in a loop would otherwise grow the buffer without limit for
        // as long as it lives. Callers retain at most `STDERR_CAPTURE_LIMIT` anyway, so reading
        // more was never useful — it was only a way to run out of memory.
        while total < STDERR_CAPTURE_LIMIT {
            let want = (STDERR_CAPTURE_LIMIT - total).min(chunk.len());
            match pipe.read(&mut chunk[..want]) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    total += n;
                    // A poisoned lock means a previous holder panicked mid-append; the bytes are
                    // still bytes, so recover the buffer rather than poison this thread too.
                    writer
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .extend_from_slice(&chunk[..n]);
                }
            }
        }
        let _ = tx.send(());
    });
    // Either the reader finished (cap or EOF) or the budget expired. Either way the answer is
    // whatever has been buffered by now — a truncated head still names the failure; nothing does not.
    let _ = rx.recv_timeout(budget);
    let bytes = buf.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&bytes).into_owned()
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
    ///
    /// The wrapper is per-platform; the SCRIPT is not. Use [`shell_seq_seat`] whenever the script
    /// is more than one command.
    fn shell_seat(key: &str, script: &str) -> AgenticCli {
        if cfg!(windows) {
            seat(key, "cmd", &format!("cmd /C \"{script}\""))
        } else {
            seat(key, "sh", &format!("sh -c \"{script}\""))
        }
    }

    /// A shell seat running `commands` in order, joined with the host shell's separator.
    ///
    /// `sh` sequences with `;`; `cmd` does not — it treats `;` as ordinary text. So
    /// `"echo needle 1>&2; exit 3"` under `cmd /C` echoes the whole string *including* the
    /// `; exit 3` and then exits **0**. The failure that produces is maximally unhelpful: the seat
    /// succeeds, parses a vote, and the assertion that fires is "this seat must not vote" — which
    /// names nothing about shell syntax. Joining here keeps the separator in one place.
    fn shell_seq_seat(key: &str, commands: &[&str]) -> AgenticCli {
        let sep = if cfg!(windows) { " & " } else { "; " };
        shell_seat(key, &commands.join(sep))
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
        let cli = shell_seq_seat("boom", &["echo diagnostic-needle 1>&2", "exit 3"]);
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
    fn the_timeout_path_returns_within_the_budget() {
        // The budget is a promise about wall clock, and the old timeout path did not keep it:
        // after killing the seat it read the pipes to EOF, which waits on every process still
        // holding the write end. Measured live at 72s under a 30s budget.
        let cli = if cfg!(windows) {
            seat("slow", "cmd", "cmd /C \"ping -n 30 127.0.0.1\"")
        } else {
            shell_seat("slow", "sleep 30")
        };
        let started = Instant::now();
        let f = failure_of(&cli, Duration::from_millis(250));
        let elapsed = started.elapsed();
        assert_eq!(f.kind, SeatFailureKind::TimedOut);
        // Budget + the drain bound + slack for process teardown on a loaded CI box. Deliberately
        // loose: the defect being guarded overran by 42 SECONDS, so anything in this range proves
        // the bound holds while leaving no room for that class of regression to slip through.
        let ceiling = Duration::from_millis(250) + DRAIN_BUDGET + Duration::from_secs(5);
        assert!(
            elapsed < ceiling,
            "the timeout path must return within its own budget: took {elapsed:?}, ceiling {ceiling:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_surviving_grandchild_cannot_extend_the_budget() {
        // The exact live shape: an agentic CLI shells out, the grandchild inherits the pipes, and
        // killing the direct child leaves EOF pending on a process we never signalled. `sh` exits
        // as soon as it is killed, but the backgrounded `sleep` holds stdout/stderr open for 20s.
        //
        // Unix-only because the fix has two halves and only one is portable: the process-group
        // kill (which makes the grandchild actually die) needs `process_group`, while the drain
        // bound (which makes the budget hold regardless) is covered on every platform by the test
        // above.
        let cli = shell_seat("forker", "sh -c 'sleep 20' & wait");
        let started = Instant::now();
        let f = failure_of(&cli, Duration::from_millis(250));
        let elapsed = started.elapsed();
        assert_eq!(f.kind, SeatFailureKind::TimedOut);
        assert!(
            elapsed < Duration::from_secs(8),
            "a grandchild holding the pipes must not extend the budget: took {elapsed:?}"
        );
    }

    #[test]
    fn the_engine_budget_matches_the_documented_default() {
        // The engine hardcoded 30s while this crate documented 60s/120s, and the gap is the
        // finding. `from_env` with nothing set is the one place both now come from.
        let d = RealDispatcher::from_env();
        let expected = RealDispatcher::default();
        // Guarded so a developer with the override exported does not see a spurious failure.
        if std::env::var(ENV_TIMEOUT_SECS).is_err() {
            assert_eq!(d.timeout, expected.timeout);
        }
        if std::env::var(ENV_LOCAL_TIMEOUT_SECS).is_err() {
            assert_eq!(d.local_runner_timeout, expected.local_runner_timeout);
        }
        assert!(
            expected.timeout >= Duration::from_secs(60),
            "the shipped roster answers a ballot in 21.5-35.5s; a budget at or below that kills \
             seats mid-reasoning and reports it as their failure"
        );
    }

    #[test]
    fn a_misconfigured_budget_falls_back_instead_of_killing_every_seat() {
        let fallback = Duration::from_secs(60);
        assert_eq!(
            secs_or(Some("90".into()), fallback),
            Duration::from_secs(90)
        );
        assert_eq!(
            secs_or(Some(" 90 ".into()), fallback),
            Duration::from_secs(90),
            "a value pasted with whitespace is still a value"
        );
        assert_eq!(secs_or(None, fallback), fallback);
        assert_eq!(secs_or(Some("".into()), fallback), fallback);
        assert_eq!(secs_or(Some("abc".into()), fallback), fallback);
        assert_eq!(secs_or(Some("-5".into()), fallback), fallback);
        assert_eq!(
            secs_or(Some("0".into()), fallback),
            fallback,
            "zero would kill every seat on spawn and report a roster-wide timeout"
        );
    }

    #[test]
    fn the_seat_ceiling_falls_back_rather_than_deadlocking() {
        assert_eq!(max_concurrent_seats(Some("6".into())), 6);
        assert_eq!(max_concurrent_seats(Some(" 6 ".into())), 6);
        assert_eq!(
            max_concurrent_seats(None),
            DEFAULT_MAX_CONCURRENT_SEATS,
            "unset means the default, not unbounded"
        );
        assert_eq!(
            max_concurrent_seats(Some("abc".into())),
            DEFAULT_MAX_CONCURRENT_SEATS
        );
        assert_eq!(
            max_concurrent_seats(Some("-1".into())),
            DEFAULT_MAX_CONCURRENT_SEATS
        );
        assert_eq!(
            max_concurrent_seats(Some("0".into())),
            DEFAULT_MAX_CONCURRENT_SEATS,
            "a ceiling of zero is not 'no limit', it is every seat blocking forever"
        );
    }

    #[test]
    fn seat_permits_bound_concurrency_and_survive_a_panicking_holder() {
        let permits = SeatPermits {
            free: std::sync::Mutex::new(2),
            returned: std::sync::Condvar::new(),
        };
        let peak = std::sync::Mutex::new(0usize);
        let live = std::sync::Mutex::new(0usize);

        std::thread::scope(|scope| {
            for i in 0..6 {
                scope.spawn(|| {
                    let _p = permits.acquire();
                    {
                        let mut n = live.lock().unwrap();
                        *n += 1;
                        let mut hi = peak.lock().unwrap();
                        *hi = (*hi).max(*n);
                    }
                    std::thread::sleep(Duration::from_millis(40));
                    *live.lock().unwrap() -= 1;
                });
                // One holder unwinds mid-dispatch. Its permit must come back, or the remaining
                // seats block forever and the council never resolves - a worse failure than the
                // contention the ceiling exists to prevent.
                if i == 2 {
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let _p = permits.acquire();
                        panic!("seat exploded");
                    }));
                }
            }
        });

        assert!(
            *peak.lock().unwrap() <= 2,
            "the ceiling must hold: peaked at {}",
            *peak.lock().unwrap()
        );
        assert_eq!(
            *permits.free.lock().unwrap(),
            2,
            "every permit must be back, including the panicking holder's"
        );
    }

    #[test]
    fn a_queued_seat_is_not_charged_for_waiting() {
        // Two slots, three seats: the third must wait for one of the first two to finish. Its
        // wall clock therefore covers a queue wait it did not choose and a run it did. Only the
        // run is budgeted, and only the run says anything about how fast that CLI is - so the
        // two must come back separately, not summed.
        //
        // Every threshold below sits MIDWAY between the two outcomes it separates, never on the
        // boundary. Timings are measured, so a threshold equal to the expected value decides on
        // scheduler jitter and `as_millis` truncation, not on behaviour: this test failed on a CI
        // runner reading 119ms for a wait of exactly RUN. Midpoints leave RUN/2 of slack on both
        // sides, which is the widest margin the two hypotheses allow.
        //
        // The run-time assertion is additionally SELF-CALIBRATING: it compares the queued seat's
        // run against the UNQUEUED seats' runs, measured on the same machine in the same scope,
        // rather than against a constant derived from RUN. Every seat sleeps the same RUN, so
        // whatever the scheduler adds on top is common-mode and cancels. A constant does not
        // cancel it - `ran < RUN * 3/2` failed on a contended CI runner at ran=333ms for
        // RUN=200ms, i.e. the runner's fixed overhead ate a margin sized for a quiet machine and
        // the test reported the summed-timing bug against code that does not have it.
        let permits = SeatPermits {
            free: std::sync::Mutex::new(2),
            returned: std::sync::Condvar::new(),
        };
        const RUN: Duration = Duration::from_millis(300);

        let mut timed: Vec<(u64, u64)> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..3)
                .map(|_| {
                    scope.spawn(|| {
                        let queue_started = Instant::now();
                        let (queued, ran) = {
                            let _permit = permits.acquire();
                            let queued = queue_started.elapsed();
                            let run_started = Instant::now();
                            std::thread::sleep(RUN);
                            (queued, run_started.elapsed())
                        };
                        (queued.as_millis() as u64, ran.as_millis() as u64)
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        // A seat that got a permit immediately waits ~0; the one that queued waits ~RUN. Longest
        // wait first, so the head IS the queued seat and the tail is the comparison group.
        timed.sort_by_key(|(queued, _)| std::cmp::Reverse(*queued));
        let (&(queued_wait, queued_ran), unqueued) = timed
            .split_first()
            .expect("three seats were spawned, so three timings come back");
        let run_ms = RUN.as_millis() as u64;
        assert!(
            queued_wait > run_ms / 2,
            "two permits and three seats: one seat must have queued for about a run: {timed:?}"
        );
        assert!(
            unqueued.iter().all(|(queued, _)| *queued <= run_ms / 2),
            "only one seat should have queued: {timed:?}"
        );

        // Correct: the queued seat's run matches the unqueued seats' - it slept the same RUN.
        // Summed (the bug): it exceeds them by its whole queue wait, ~RUN more. Half the measured
        // wait is the midpoint between those two outcomes, and it scales with what was measured
        // rather than with what was expected.
        let baseline = unqueued
            .iter()
            .map(|(_, ran)| *ran)
            .max()
            .expect("two seats ran without queueing");
        assert!(
            queued_ran < baseline + queued_wait / 2,
            "run time must exclude the queue wait: the queued seat ran={queued_ran}ms after \
             waiting {queued_wait}ms, while unqueued seats ran at most {baseline}ms ({timed:?})"
        );
    }

    /// Block — with NO budget — until the child has actually written its first byte.
    ///
    /// Spawning a process out of a large, heavily-threaded test binary is not instant: measured at
    /// ~200ms idle on this machine, and seconds when every core is busy. Both drain tests are about
    /// what `drain_stderr` does with bytes that EXIST, not about how promptly the OS produces them,
    /// so waiting here takes process-spawn latency out of the timed assertion that follows. Leaving
    /// it in made both tests fail under a saturated machine for a reason neither one is testing.
    ///
    /// The byte is consumed, not peeked (a pipe has no unread). Callers assert on what comes after.
    #[cfg(unix)]
    fn block_until_writing(child: &mut std::process::Child) {
        use std::io::Read;
        let mut first = [0u8; 1];
        child
            .stderr
            .as_mut()
            .expect("the child was spawned with stderr piped")
            .read_exact(&mut first)
            .expect("the writer must produce at least one byte");
    }

    #[test]
    #[cfg(unix)]
    fn the_drain_stops_reading_at_the_cap_not_at_eof() {
        // The drain thread outlives the bounded WAIT: `drain_stderr` returns after its budget,
        // but the thread it spawned keeps reading. A grandchild that survived the group kill and
        // writes in a loop would grow that buffer for as long as it lives, so the READ has to be
        // bounded too - not just the wait. Callers retain at most STDERR_CAPTURE_LIMIT anyway.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("yes 0123456789 1>&2")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        {
            use std::os::unix::process::CommandExt;
            // Its own group, exactly as `run_in_isolation` spawns a seat. Without this the child
            // sits in the TEST RUNNER's group and `kill_process_tree` would signal us.
            command.process_group(0);
        }
        let mut child = command.spawn().expect("spawn a writer that never stops");
        block_until_writing(&mut child);

        let started = Instant::now();
        let drained = drain_stderr(&mut child, Duration::from_secs(5));
        let elapsed = started.elapsed();
        // Reap BEFORE asserting: a failing assertion unwinds, and a leaked `yes` outlives the
        // whole test binary.
        kill_process_tree(&mut child);
        let _ = child.wait();

        // Elapsed is in the message on purpose. The two ways this can fail are not the same bug:
        // returning fast with nothing means the read was discarded, returning at the full budget
        // with nothing means the writer never got scheduled. Without the timing they look alike.
        assert!(
            !drained.is_empty(),
            "an endless writer must still yield its head (returned empty after {elapsed:?})"
        );
        assert!(
            drained.len() <= STDERR_CAPTURE_LIMIT,
            "the read must stop at the cap, got {} bytes",
            drained.len()
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_writer_that_never_closes_still_yields_what_it_already_wrote() {
        // The budget bounds the WAIT, not the yield. A seat that logged its error and then hung
        // (a hung retry loop, a child holding the pipe open) reaches neither EOF nor the cap, so
        // the drain always spends its whole budget here — and the head it wrote is exactly the
        // diagnostic the caller needs. An all-or-nothing hand-off returns the empty string for
        // this process, silently converting a named failure into an unexplained one.
        let mut command = Command::new("sh");
        command
            .arg("-c")
            // `%s` rather than an interpolated format string: shells differ on whether `printf`
            // expands escapes in the FORMAT argument, and stdout is /dev/null here, so the
            // `1>&2` is what puts the line on the pipe being drained. The leading `.` is the
            // sentinel `block_until_writing` consumes.
            .arg("printf '%s\\n' '.BOOM: seat could not start' 1>&2; sleep 30")
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = command.spawn().expect("spawn a writer that then hangs");
        block_until_writing(&mut child);

        let started = Instant::now();
        let drained = drain_stderr(&mut child, Duration::from_secs(3));
        let elapsed = started.elapsed();
        kill_process_tree(&mut child);
        let _ = child.wait();

        assert!(
            drained.contains("BOOM: seat could not start"),
            "a timed-out drain must still surface what was written, got {drained:?} after {elapsed:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn killing_a_child_that_leads_no_group_does_not_kill_us() {
        // A child spawned WITHOUT `process_group(0)` inherits this process's group. Handing its
        // pid to `killpg` would SIGKILL that group - this test binary included - so the guard in
        // `kill_process_tree` has to notice. There is no way to mutation-check this one by
        // letting it fail: without the guard the process dies outright rather than reporting.
        let mut child = Command::new("sh")
            .arg("-c")
            .arg("sleep 30")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn a child in our own group");

        assert_eq!(
            unsafe { sig::getpgid(child.id() as i32) },
            unsafe { sig::getpgid(0) },
            "fixture precondition: the child must share OUR group"
        );

        kill_process_tree(&mut child);
        let reaped = child.wait().expect("the direct child still dies");

        // Reaching this line at all is the assertion: the guard held and we were not signalled.
        assert!(!reaped.success(), "killed, not exited cleanly");
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
