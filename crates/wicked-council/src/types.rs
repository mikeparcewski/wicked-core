//! Council domain types + the seam traits — ported from the standalone `council-core`
//! crate onto the wicked-apps spine.
//!
//! Types + traits only. **No behavior, no I/O, no subprocess, no SQLite, no bus.**
//! In the original repo these lived in a locked `council-core` crate; here they fold
//! into the `wicked-council` lib (the wicked-apps workspace already locks its spine in
//! `wicked-apps-core`). Fields use only `String`/`Vec`/`Option` + small enums so the types
//! carry no premature runtime dependency.
//!
//! The three bus events this app produces are mirrored in `wicked-apps-core`
//! (`EV_COUNCIL_REQUESTED` / `EV_COUNCIL_VOTED` / `EV_CLI_RANKED`); [`COUNCIL_EVENTS`]
//! re-states them here so the engine can enumerate its own contract.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The three bus events this app **produces**, per the shared catalog
/// (`wicked-apps-core`: `EV_COUNCIL_REQUESTED` / `EV_COUNCIL_VOTED` / `EV_CLI_RANKED`).
pub const COUNCIL_EVENTS: [&str; 3] = [
    wicked_apps_core::EV_COUNCIL_REQUESTED,
    wicked_apps_core::EV_COUNCIL_VOTED,
    wicked_apps_core::EV_CLI_RANKED,
];

// ---------------------------------------------------------------------------
// Enums (small, serde-friendly classifiers)
// ---------------------------------------------------------------------------

/// What kind of CLI seat this is. Local runners get a longer dispatch timeout
/// (cold model load).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    /// An agentic coding CLI (claude, codex, gemini, …).
    #[default]
    AgenticCoder,
    /// A chat-style CLI (llm, aichat, mods, …).
    Chat,
    /// A local model runner (ollama, …) — slower cold start.
    LocalRunner,
}

/// Wire transport for an ACP (Agent Client Protocol) server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AcpTransport {
    /// JSON-RPC 2.0 ndjson over stdin/stdout — spawn the binary, pipe I/O.
    #[default]
    Stdio,
    /// JSON-RPC 2.0 via HTTP POST + SSE — spawn the binary with port args, connect via HTTP.
    Http,
}

/// ACP server configuration for a CLI seat.
///
/// When set on an [`AgenticCli`], the engine attempts an ACP multi-turn session
/// before falling back to single-shot invocation. A startup failure (binary not found,
/// handshake error) emits a warning in the step output and triggers the fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcpConfig {
    /// The binary that implements the ACP server protocol.
    /// May differ from the CLI binary itself (e.g. `"claude-agent-acp"` for claude,
    /// `"codex-acp"` for codex). For HTTP-mode CLIs this is the CLI binary itself.
    pub binary: String,
    /// Extra args passed to start the ACP server. Empty for stdio-based servers.
    /// For HTTP-transport CLIs: e.g. `["--acp", "--port", "3001"]`.
    #[serde(default)]
    pub start_args: Vec<String>,
    /// Wire transport to use when connecting to this ACP server.
    #[serde(default)]
    pub transport: AcpTransport,
}

/// How the scaffold prompt is delivered to the CLI process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputMode {
    /// Substitute the prompt into `{PROMPT}` in `headless_invocation`.
    #[default]
    PromptArg,
    /// Pipe the prompt on stdin (template should read stdin).
    Stdin,
    /// Attach the prompt as a file referenced by `{PROMPT}` (path substituted).
    AtFile,
    /// Attach the prompt as a message file referenced by `{PROMPT}` (path substituted).
    MessageFile,
    /// Keep the CLI process alive as a persistent PTY session; write each turn's prompt to stdin
    /// and detect completion via NDJSON `{"type":"result"}` parsing. Enables prompt-cache reuse
    /// across governance-gated turns within the same run (wicked-core#13).
    PtySession,
}

/// How much we trust the record's `headless_invocation` before relying on it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Confidence {
    /// Shipped + hand-verified flags.
    Verified,
    /// User-supplied or uncertain — the probe must confirm the headless flag first.
    #[default]
    ConfirmOnProbe,
}

/// Why a detected CLI is **not** a usable seat. Ordered roughly by how the probe
/// classifies combined stdout+stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UnusableReason {
    /// 401/403, "not logged in", "invalid api key", "re-authenticate".
    Auth,
    /// "no provider configured", "set …_API_KEY", "run … configure".
    NoProvider,
    /// "connection refused", "is the server running", "no such model".
    DaemonDown,
    /// "rate limit", 429, "insufficient credits", 402.
    Quota,
    /// The per-CLI deadline elapsed.
    Timeout,
    /// Not detected on PATH at all.
    NotFound,
    /// Non-zero exit / unrecognised signature (never silently trusted).
    Error,
}

/// The lifecycle state of a queued council, mirrored in the durable store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TaskState {
    /// Persisted, not yet picked up by the worker.
    Queued,
    /// The detached worker is dispatching CLIs.
    Running,
    /// A verdict was synthesized.
    Voted,
    /// The deadline elapsed before enough votes landed.
    TimedOut,
    /// The council could not run (e.g. no usable CLIs).
    Failed,
}

impl TaskState {
    /// The lowercase wire string for this state (used in node metadata).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Queued => "queued",
            TaskState::Running => "running",
            TaskState::Voted => "voted",
            TaskState::TimedOut => "timed-out",
            TaskState::Failed => "failed",
        }
    }

    /// Parse a state from its wire string.
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "queued" => Some(TaskState::Queued),
            "running" => Some(TaskState::Running),
            "voted" => Some(TaskState::Voted),
            "timed-out" => Some(TaskState::TimedOut),
            "failed" => Some(TaskState::Failed),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// A registry record for one agentic/chat/local-LLM CLI seat.
///
/// This is the de-drift source of truth: flags are encoded here, never re-derived
/// per call. Built-in records ship `Verified`; user TOML records default to
/// `ConfirmOnProbe`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgenticCli {
    /// Stable registry key (e.g. "claude", "agy").
    pub key: String,
    /// Human-facing name.
    pub display_name: String,
    /// The binary name resolved on `PATH`.
    pub binary: String,
    /// Headless invocation template (contains `{PROMPT}`).
    pub headless_invocation: String,
    /// What kind of seat this is.
    #[serde(default)]
    pub category: Category,
    /// How the prompt is delivered.
    #[serde(default)]
    pub input_mode: InputMode,
    /// argv that prints a version (collision disambiguation). Empty = skip probe.
    #[serde(default)]
    pub version_probe: Vec<String>,
    /// Flags appended for headless runs so the CLI never blocks on a prompt.
    #[serde(default)]
    pub trust_flags: Vec<String>,
    /// Alternate binary names to also scan on PATH.
    #[serde(default)]
    pub alt_binaries: Vec<String>,
    /// Trust level for the headless flag before the council relies on it.
    #[serde(default)]
    pub confidence: Confidence,
    /// Whether this seat may be convened.
    #[serde(default = "default_true")]
    pub enabled_for_council: bool,
    /// ACP multi-turn session config. When present, the engine tries ACP first and falls
    /// back to single-shot invocation if the ACP server is unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp: Option<AcpConfig>,
    /// Human-readable capability profile for this seat — what kinds of tasks it excels at.
    /// Used by the council as the option label voters see; CLI names are never exposed.
    /// Example: "broad reasoning, TypeScript/React, refactoring, API design"
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A council request: a topic, the options under consideration, and the criteria.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilTask {
    /// Task identifier (sortable id string assigned by the engine).
    pub id: String,
    /// The decision topic.
    pub topic: String,
    /// The options being weighed.
    pub options: Vec<String>,
    /// The evaluation criteria (e.g. "blast-radius", "operational-cost").
    pub criteria: Vec<String>,
    /// The requesting agent's session id.
    pub session_id: String,
}

/// The outcome of a two-stage probe of one CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeOutcome {
    /// The CLI key probed.
    pub cli: String,
    /// Whether the CLI is a usable council seat (detected AND answered).
    pub usable: bool,
    /// Why it is unusable, if it is not.
    pub reason: Option<UnusableReason>,
    /// The resolved path on PATH, if detected.
    pub resolved_path: Option<String>,
    /// The captured version string, if a version probe ran.
    pub version: Option<String>,
}

/// One CLI's answer to the fixed 4-question scaffold.
///
/// Confidence is **never** an averaged model number — consensus is measured by risk
/// convergence. `provenance` records which CLI/version/isolation produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Vote {
    /// The CLI that produced this vote.
    pub cli: String,
    /// The recommended option + trade-offs.
    pub recommendation: String,
    /// The single biggest risk in the recommendation.
    pub top_risk: String,
    /// The evidence/condition that would reverse it.
    pub change_my_mind: String,
    /// Any option deemed fundamentally unviable (None = all viable).
    pub disqualifier: Option<String>,
    /// The CLI's self-reported confidence label (carried, never averaged into the verdict).
    #[serde(default)]
    pub confidence: Confidence,
    /// Which CLI, which version, run under what isolation.
    pub provenance: String,
}

/// The synthesized council verdict for a task.
///
/// `kind` is the copy-pasteable summary string ("Consensus: A (2/2)" /
/// "NoConsensus: A vs B"); structured fields carry the machine-readable shape.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    /// The task this verdict belongs to.
    pub task_id: String,
    /// Human/machine summary: "Consensus" | "NoConsensus" prefix.
    pub kind: String,
    /// `true` when a strict majority of votes converge on one recommendation.
    pub consensus: bool,
    /// The recommendation the most votes converged on (the winner), if any.
    pub winning_recommendation: Option<String>,
    /// Agreement ratio in `[0.0, 1.0]`: winning vote count / total votes.
    /// Emitted on `wicked.council.voted`. Counts agreement, NOT averaged confidence.
    pub agreement_ratio: f32,
    /// Risk convergence: each distinct `top_risk` and how many CLIs cited it,
    /// most-cited first. The high-signal axis.
    pub risk_convergence: Vec<(String, u32)>,
    /// Recommendations cited by a minority (the dissent / fault lines).
    pub dissent: Vec<String>,
}

/// A per-`(cli × work-kind)` ranking entry returned by [`RankStore::best_for`].
///
/// Carries a score **and provenance** — never a bare number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Ranking {
    /// The CLI key.
    pub cli: String,
    /// The work-kind this ranking is for.
    pub work_kind: String,
    /// Score in `[0.0, 1.0]` — a success-rate signal, not a model confidence.
    pub score: f32,
    /// Number of observations behind the score (cold-start honesty).
    pub n: u32,
    /// Human-readable provenance ("agreement_with_consensus↑, latency↓").
    pub provenance: String,
}

/// One outcome observation recorded after a council, per participating CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankSignal {
    /// Did the CLI produce a usable vote?
    pub success: bool,
    /// Did the CLI's recommendation agree with the eventual consensus?
    pub agreement_with_consensus: bool,
    /// How long the dispatch took.
    pub latency_ms: u64,
}

// ---------------------------------------------------------------------------
// Traits (the seams — real impls live in the engine; tests inject fakes)
// ---------------------------------------------------------------------------

/// Stage-2 usability probe: does this CLI actually answer (not merely exist)?
///
/// The real implementor shells a subprocess; tests inject a fake so `cargo test`
/// stays offline + deterministic.
pub trait Prober {
    /// Probe one CLI; returns the classified outcome.
    fn probe(&self, cli: &AgenticCli) -> ProbeOutcome;
}

/// A council seat's deliberation identity — the unique lens a voter evaluates through,
/// like a named chair on a real review board. Assigned deterministically per convened
/// CLI so re-runs are reproducible; the voter is told its seat, never its CLI identity.
/// Prompt-rendering input only — never persisted or serialized (hence no serde derives;
/// `&'static str` fields cannot meaningfully round-trip through deserialization).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seat {
    /// Short seat name shown in the prompt (e.g. "Capability Fit").
    pub name: &'static str,
    /// The evaluation lens the seat is asked to prioritize.
    pub lens: &'static str,
}

/// The built-in seat rotation. Extra convened CLIs wrap around (two "Capability Fit"
/// seats on a 5+-seat council is fine — perspectives bias, they don't partition).
pub const SEATS: &[Seat] = &[
    Seat {
        name: "Capability Fit",
        lens: "Does the profile's core strength actually match the primary work in this task? Weigh demonstrated fit over generality.",
    },
    Seat {
        name: "Risk & Failure Modes",
        lens: "Which profile is least likely to fail, stall, or produce something unusable on this task? Weigh downside over upside.",
    },
    Seat {
        name: "Efficiency",
        lens: "Which profile completes this task with the least wasted time and cost? Weigh directness and turnaround.",
    },
    Seat {
        name: "Output Quality",
        lens: "Which profile produces the most correct, reviewable, and complete artifact for this task? Weigh craft over speed.",
    },
];

/// Context for one deliberation ballot: which seat the voter holds, which ballot round
/// this is, the approval bar, and — on runoff rounds — the prior tally + dissent so the
/// council can converge like a real deliberating body instead of re-rolling blind.
#[derive(Debug, Clone)]
pub struct BallotContext {
    /// The seat this voter holds (None = unassigned / legacy single-shot path).
    pub seat: Option<Seat>,
    /// 1-based ballot number (1 = first ballot, >1 = runoff).
    pub ballot: u32,
    /// The approval share the council must reach in `[0.0, 1.0]` (e.g. `0.75`).
    /// `0.0` means no bar is stated in the prompt (legacy scaffold).
    pub approval_threshold: f32,
    /// Runoff only: the prior ballot's tally lines, most-voted first (display, count).
    pub prior_tally: Vec<(String, u32)>,
    /// Runoff only: anonymized dissent arguments (top risks cited by non-winning votes).
    pub dissent_arguments: Vec<String>,
}

/// The legacy plain-scaffold context: no seat, first ballot, no approval bar. `ballot`
/// is 1 (the field is documented 1-based; a derived `Default` would set the invalid 0).
impl Default for BallotContext {
    fn default() -> Self {
        BallotContext {
            seat: None,
            ballot: 1,
            approval_threshold: 0.0,
            prior_tally: Vec::new(),
            dissent_arguments: Vec::new(),
        }
    }
}

/// Why one seat produced no vote.
///
/// The dispatch path has ten distinct ways to yield no vote and used to collapse all of them
/// into a bare `None`, which `distribute.rs` then rendered as the single string "council did not
/// reach a vote". That string is the same whether the binary is missing, the CLI exited non-zero,
/// or the seat was skipped outright — so a 92.6% degradation rate was undiagnosable by
/// construction. Naming the branch is what makes it diagnosable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatFailureKind {
    /// `headless_invocation` tokenized to nothing, or yielded no program token.
    InvocationEmpty,
    /// `InputMode::PtySession`: the council dispatcher does not manage PTY sessions, so the
    /// seat is skipped before any process is spawned. Structurally incapable of voting.
    PtyUnsupported,
    /// The per-dispatch isolation tempdir could not be created.
    WorkdirUnavailable,
    /// The prompt file could not be written into the isolation dir.
    PromptWriteFailed,
    /// The process could not be spawned — missing binary, not executable, permissions.
    SpawnFailed,
    /// The process outlived the dispatch budget and was killed.
    TimedOut,
    /// Waiting on the process failed.
    WaitFailed,
    /// The process ran to completion and exited non-zero.
    NonZeroExit,
    /// The dispatcher reported no vote without saying why. Test stubs and any implementation
    /// that has not adopted [`Dispatcher::dispatch_ballot_detailed`] land here — it records
    /// "not reported", which is honest, rather than inventing a cause.
    Unreported,
}

impl SeatFailureKind {
    /// Stable snake_case token for events and degrade reasons.
    pub fn as_str(self) -> &'static str {
        match self {
            SeatFailureKind::InvocationEmpty => "invocation_empty",
            SeatFailureKind::PtyUnsupported => "pty_unsupported",
            SeatFailureKind::WorkdirUnavailable => "workdir_unavailable",
            SeatFailureKind::PromptWriteFailed => "prompt_write_failed",
            SeatFailureKind::SpawnFailed => "spawn_failed",
            SeatFailureKind::TimedOut => "timed_out",
            SeatFailureKind::WaitFailed => "wait_failed",
            SeatFailureKind::NonZeroExit => "non_zero_exit",
            SeatFailureKind::Unreported => "unreported",
        }
    }
}

/// The captured diagnostics for one seat that failed to vote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatFailure {
    /// Which of the dispatch branches was taken.
    pub kind: SeatFailureKind,
    /// The process exit code, when the process ran to completion.
    pub exit_code: Option<i32>,
    /// Captured stderr, truncated to [`STDERR_CAPTURE_LIMIT`]. `run_in_isolation` already
    /// piped stderr and then dropped it on the floor; this is that artifact, kept.
    pub stderr: String,
    /// The OS/IO error text, where the branch has one.
    pub detail: String,
}

/// Cap on retained stderr per seat. Enough to carry a usage message or a stack trace's head,
/// bounded so a runaway CLI cannot balloon an event payload or the task record.
pub const STDERR_CAPTURE_LIMIT: usize = 4096;

impl SeatFailure {
    /// A failure with no captured process output — the pre-spawn branches.
    pub fn new(kind: SeatFailureKind, detail: impl Into<String>) -> Self {
        SeatFailure {
            kind,
            exit_code: None,
            stderr: String::new(),
            detail: detail.into(),
        }
    }

    /// Attach captured stderr, truncated on a char boundary so the result stays valid UTF-8.
    pub fn with_stderr(mut self, stderr: &str) -> Self {
        let end = if stderr.len() <= STDERR_CAPTURE_LIMIT {
            stderr.len()
        } else {
            // `floor_char_boundary` is unstable; walk back to the nearest boundary ourselves.
            let mut i = STDERR_CAPTURE_LIMIT;
            while i > 0 && !stderr.is_char_boundary(i) {
                i -= 1;
            }
            i
        };
        self.stderr = stderr[..end].to_string();
        self
    }

    /// One-line reason suitable for a degrade string: the branch, plus the most specific
    /// evidence available for it.
    pub fn reason(&self) -> String {
        let mut s = self.kind.as_str().to_string();
        if let Some(code) = self.exit_code {
            s.push_str(&format!(" (exit {code})"));
        }
        // stderr is the more specific artifact when both are present — it is the CLI's own words.
        let evidence = if !self.stderr.is_empty() {
            self.stderr.trim()
        } else {
            self.detail.trim()
        };
        if !evidence.is_empty() {
            // Degrade strings land in single-line contexts (events, logs, the studio).
            let flat: String = evidence.split_whitespace().collect::<Vec<_>>().join(" ");
            s.push_str(": ");
            s.push_str(&flat);
        }
        s
    }
}

/// One seat's dispatch result: a vote, or the named reason there is none.
///
/// The invariant is that exactly one side is populated — a `None` vote always carries a
/// `SeatFailure`, even if only [`SeatFailureKind::Unreported`]. That is what stops a silent
/// no-vote from re-entering the system.
#[derive(Debug, Clone)]
pub struct DispatchOutcome {
    /// The vote, when the seat produced one.
    pub vote: Option<Vote>,
    /// Why there is no vote. `Some` exactly when `vote` is `None`.
    pub failure: Option<SeatFailure>,
}

impl DispatchOutcome {
    /// A successful ballot.
    pub fn voted(vote: Vote) -> Self {
        DispatchOutcome {
            vote: Some(vote),
            failure: None,
        }
    }

    /// A named failure.
    pub fn failed(failure: SeatFailure) -> Self {
        DispatchOutcome {
            vote: None,
            failure: Some(failure),
        }
    }

    /// Lift a legacy `Option<Vote>`. A bare `None` becomes [`SeatFailureKind::Unreported`]
    /// rather than an empty failure, so the "no vote ⇒ some reason" invariant holds even for
    /// dispatchers that never adopted the detailed path.
    pub fn from_option(vote: Option<Vote>) -> Self {
        match vote {
            Some(v) => DispatchOutcome::voted(v),
            None => DispatchOutcome::failed(SeatFailure::new(
                SeatFailureKind::Unreported,
                "dispatcher returned no vote and reported no reason",
            )),
        }
    }
}

/// Isolated, timeboxed dispatch of the 4-question scaffold to one CLI.
pub trait Dispatcher {
    /// Dispatch the scaffold to one CLI and collect its vote (`None` on failure/timeout).
    fn dispatch(&self, cli: &AgenticCli, task: &CouncilTask) -> Option<Vote>;

    /// Dispatch one deliberation ballot with seat + round context. The default ignores
    /// the context and delegates to [`Dispatcher::dispatch`], so existing implementations
    /// (test stubs, fakes) keep working unchanged; the real dispatcher overrides this to
    /// render the seat lens, approval bar, and runoff tally into the prompt.
    fn dispatch_ballot(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        _ctx: &BallotContext,
    ) -> Option<Vote> {
        self.dispatch(cli, task)
    }

    /// Dispatch one ballot and report *why* on failure.
    ///
    /// Extension point, added the same way `dispatch_ballot` was: the default delegates to
    /// [`Dispatcher::dispatch_ballot`] and labels a bare `None` as
    /// [`SeatFailureKind::Unreported`], so every existing implementation keeps compiling
    /// unchanged. The real dispatcher overrides it to name the branch and carry the CLI's
    /// stderr out.
    fn dispatch_ballot_detailed(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        ctx: &BallotContext,
    ) -> DispatchOutcome {
        DispatchOutcome::from_option(self.dispatch_ballot(cli, task, ctx))
    }
}

/// Per-`(cli × work-kind)` ranking memory.
///
/// Here the impl is an estate-store projection (one `CLI_RANKING` node per pair).
pub trait RankStore {
    /// Record an outcome signal for a CLI on a kind of work.
    fn record(&self, cli: &str, work_kind: &str, signal: &RankSignal);
    /// Return the top-N rankings for a kind of work, best first.
    fn best_for(&self, work_kind: &str, top: usize) -> Vec<Ranking>;
}

/// Event emission seam (the `wicked-bus` adapter); **degrades to no-op if absent**.
pub trait EventSink {
    /// Emit an event by name with a JSON payload. Fire-and-forget.
    fn emit(&self, event: &str, payload: &serde_json::Value);
}

/// A trivial no-op [`EventSink`] used when the bus is absent (degrade cleanly).
#[derive(Debug, Default, Clone)]
pub struct NoopEventSink;

impl EventSink for NoopEventSink {
    fn emit(&self, _event: &str, _payload: &serde_json::Value) {}
}

/// Helper kept on the spine so install-hints round-trip in the registry record
/// without forcing the engine to know the map shape. Empty by default.
pub type InstallHints = BTreeMap<String, String>;
