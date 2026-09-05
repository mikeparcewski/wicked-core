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
//! (`EV_COUNCIL_REQUESTED` / `EV_COUNCIL_DELIBERATED` / `EV_COUNCIL_SEAT_FAILED` /
//! `EV_COUNCIL_VOTED` / `EV_CLI_RANKED`); [`COUNCIL_EVENTS`] re-states them here so the engine
//! can enumerate its own contract.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The bus events this app **produces**, per the shared catalog in `wicked-apps-core`.
///
/// This is the crate's published contract, so it has to list what `worker.rs` actually emits —
/// not a subset. It had already drifted once (`EV_COUNCIL_DELIBERATED` shipped without being
/// declared here) because the test below only restated the same literals back, which no amount
/// of drift can fail. `council_events_are_the_events_the_crate_emits` now checks the list
/// against the emitting source instead.
pub const COUNCIL_EVENTS: [&str; 5] = [
    wicked_apps_core::EV_COUNCIL_REQUESTED,
    wicked_apps_core::EV_COUNCIL_DELIBERATED,
    wicked_apps_core::EV_COUNCIL_SEAT_FAILED,
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
    /// The `methodId` to send in the ACP `authenticate` call when the agent's `initialize`
    /// response advertises a non-empty `authMethods` list. When unset, the first advertised
    /// method is used (the agent's own preference order). Ignored when the agent advertises
    /// no methods — `authenticate` is never sent unsolicited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_method: Option<String>,
    /// Whether this ACP adapter has passed the evidence proof required to route every
    /// `session/request_permission` through wicked-core's shared policy and audit gate.
    ///
    /// Defaults to `false`: an adapter remains explicitly input-ungoverned until its pinned
    /// version has proved that every tool action blocks on a request with canonical tool identity
    /// and raw input, honours rejection, and has its auto-approval surface disabled. This admits
    /// an adapter to the core ACP gate; it does not claim a sandbox.
    #[serde(default)]
    pub acp_input_governance: bool,
    /// An environment variable `(name, value)` the engine sets on this seat's ACP child process,
    /// UNCONDITIONALLY, at every spawn — never gated on whether the particular unit being run is
    /// itself governed. A cached, already-spawned session cannot retroactively gain an env var
    /// once a later turn turns out to need it, so conditioning this on a per-turn governance
    /// decision would leave a governed turn running against a process spawned before governance
    /// was known to apply (DES-INPUT-GOV-006 §3.3). Exists so an adapter whose default ruleset
    /// resolves every core intent to "allow" (opencode: OQ-OPENCODE-ACP-001) can be forced to
    /// route every intent through `session/request_permission` instead, where wicked-core's own
    /// `AcpGate` answers for real. The injected value is a FORCING FUNCTION, not a policy
    /// statement: it does not need to match wicked-core's own allow/deny verdict, only to keep
    /// the adapter from resolving any core intent to "allow" before ever asking
    /// (DES-INPUT-GOV-006 §1.1). `None` for a seat needing no such injection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acp_governance_env: Option<(String, String)>,
    /// The exact `--version` output this seat's `acp_input_governance` admission was proven
    /// against (e.g. opencode: oq-opencode-acp-002). `None` for a seat admitted without a version
    /// dependency. When set, the engine re-probes the ACTUAL binary about to be spawned
    /// immediately before spawn and refuses to treat the resulting session as governed —
    /// falling back to the same disclosed-ungoverned path as `acp_input_governance: false` —
    /// if the live output does not match byte-for-byte (trimmed). Guards against an unpinned,
    /// auto-updating distribution (opencode's Homebrew tap has no lockfile) silently reopening a
    /// gap this admission closed against one specific build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_version: Option<String>,
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
    /// The INTERACTIVE command that signs this seat in — the CLI's OWN login flow, meant to be
    /// hosted in a PTY (the studio's sign-in terminal). The platform never implements provider
    /// auth itself: it runs this command, the operator completes the CLI's URL/paste flow, and
    /// the CLI writes its own credential store. `None` ⇒ fall back to the registry's built-in
    /// default for the seat key ([`default_login_invocation`]), else no sign-in surface.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub login_invocation: Option<String>,
}

/// Built-in sign-in commands for the known seat keys — used when a registry entry does not
/// override `login_invocation`. Each is the seat's OWN documented interactive flow (device-code
/// or URL+paste), so it works inside a PTY with no localhost-callback assumptions.
#[must_use]
pub fn default_login_invocation(key: &str) -> Option<&'static str> {
    match key {
        // The worker home (crew#267 option 3): sign in the ENGINE-owned config dir, not the
        // operator's — inside the REPL, `/login` runs the URL+paste flow.
        "claude" => Some(r#"CLAUDE_CONFIG_DIR="$HOME/.wicked-worker/claude" claude"#),
        "codex" => Some("codex login --device-auth"),
        "copilot" => Some("copilot login"),
        "opencode" => Some("opencode auth login"),
        "pi" => Some("pi"),
        "agy" => Some("agy"),
        _ => None,
    }
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
    /// `true` when a strict majority of the **seated** council converges on one recommendation.
    ///
    /// The denominator is [`Verdict::seated`], not the votes cast. A council of three that hears
    /// back from one is not unanimous — it is quorate-failed, and saying otherwise puts a
    /// three-seat agreement in the audit trail that never happened (FINDING-026 D).
    pub consensus: bool,
    /// Seats CONVENED for this council — the quorum denominator.
    ///
    /// Distinct from the number of votes cast: a seat that timed out is seated and did not
    /// return. Carried on the verdict so a reader never has to reconstruct the quorum by
    /// comparing a separate `returned` field against the session's roster length.
    ///
    /// `#[serde(default)]` because a `Verdict` round-trips through the estate store: records
    /// written before this field existed must still load, and 0 reads as "not recorded", which
    /// the arithmetic below treats as "no better information than the cast count".
    #[serde(default)]
    pub seated: u32,
    /// The recommendation the most votes converged on (the winner), if any.
    pub winning_recommendation: Option<String>,
    /// Agreement ratio in `[0.0, 1.0]`: winning vote count / votes that **answered** (cast a
    /// non-empty recommendation — a tolerant parse of a hollow exit-0 return is not an answer).
    ///
    /// Deliberately NOT quorum-adjusted — it answers "of the seats that answered, how many
    /// agreed?". Observability only: the runoff loop's exit is measured separately, as the
    /// winner's share of the LIVE council (`synthesis::live_agreement`, winner / seated −
    /// benched), and quorum is a third axis again, living on `consensus` + `seated`. Three
    /// denominators, three questions — this one is the conversation among those who spoke,
    /// and folding either of the others into it would misstate that.
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
    /// The dispatch itself panicked.
    ///
    /// Seats are dispatched on their own threads, so one that unwinds is caught and recorded as
    /// that seat's failure rather than propagated. The alternative — letting it reach the council
    /// thread — turns one bad seat into a failed distribution, which is the opposite of what a
    /// quorum is for.
    Panicked,
    /// The seat is benched by the dispatcher's health gate: it failed consecutively and is
    /// sitting out its backoff, so the dispatch short-circuited before spawning anything.
    ///
    /// This is an ABSTENTION, not an error: the seat was seated, was asked, and cost the ballot
    /// nothing. The council counts it separately from the failure kinds above — a benched seat
    /// shrinks the *live* majority denominator, while a timed-out seat is an answer that was
    /// lost and still counts against it. Recovery is a real ballot round-trip (a probationary
    /// dispatch on bench expiry), never a `--version` probe: a binary that prints its version is
    /// alive, not ready.
    Benched,
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
            SeatFailureKind::Panicked => "panicked",
            SeatFailureKind::Benched => "benched",
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
/// An enum, not a struct with two `Option`s: the whole point of this type is that a no-vote
/// always carries a reason — even if only [`SeatFailureKind::Unreported`] — and a pair of
/// public `Option` fields would let a caller construct the exact state the type exists to
/// forbid (both empty), reintroducing the silent no-vote through the back door.
#[derive(Debug, Clone)]
pub enum DispatchOutcome {
    /// The seat voted.
    Voted(Vote),
    /// The seat did not vote, and this is why.
    Failed(SeatFailure),
}

/// A seat's outcome with its wall clock split into the part the budget governs and the part it
/// does not.
///
/// The two must stay separate. A seat that waited two minutes for a concurrency permit and then
/// ran for sixty seconds has a two-minute wall clock, but its dispatch budget was never exceeded;
/// reporting the sum next to "exceeded 60s dispatch budget" reproduces exactly the contradiction
/// FINDING-026 was about. Queue time is also a property of how loaded the council is, not of the
/// CLI, so folding it into the ranking signal would penalise whichever seat happened to queue.
#[derive(Debug, Clone)]
pub struct TimedOutcome {
    /// What the seat returned.
    pub outcome: DispatchOutcome,
    /// Time spent waiting for a dispatch slot before the process started. Not budgeted.
    pub queued_ms: u64,
    /// Time the seat's process actually ran. This is what the dispatch budget bounds.
    pub ran_ms: u64,
}

impl DispatchOutcome {
    /// Lift a legacy `Option<Vote>`. A bare `None` becomes [`SeatFailureKind::Unreported`]
    /// rather than an empty failure, so the "no vote ⇒ some reason" invariant holds even for
    /// dispatchers that never adopted the detailed path.
    pub fn from_option(vote: Option<Vote>) -> Self {
        match vote {
            Some(v) => DispatchOutcome::Voted(v),
            None => DispatchOutcome::Failed(SeatFailure::new(
                SeatFailureKind::Unreported,
                "dispatcher returned no vote and reported no reason",
            )),
        }
    }

    /// The vote, discarding the reason — for the legacy `Option<Vote>` callers.
    pub fn into_vote(self) -> Option<Vote> {
        match self {
            DispatchOutcome::Voted(v) => Some(v),
            DispatchOutcome::Failed(_) => None,
        }
    }

    /// Whether the seat voted, without consuming the outcome.
    pub fn is_voted(&self) -> bool {
        matches!(self, DispatchOutcome::Voted(_))
    }

    /// Whether the seat cast a USABLE vote — parsed, with a non-empty recommendation.
    ///
    /// The one predicate health, ranking and telemetry share, so they cannot drift:
    /// [`DispatchOutcome::is_voted`] says a `Vote` value exists, which tolerant parsing
    /// guarantees for ANY exit-0 (a help screen, an auth banner), while this says the vote can
    /// actually count — synthesis tallies exactly the votes this accepts. A seat must never be
    /// penalized by seat health for a hollow return and simultaneously credited for it in the
    /// ranking store; that inconsistency biases future seat selection toward CLIs that exit 0
    /// without answering.
    pub fn is_usable_vote(&self) -> bool {
        matches!(self, DispatchOutcome::Voted(v) if !v.recommendation.trim().is_empty())
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

    /// Dispatch one ballot and report how long the seat *ran* separately from how long it
    /// *waited for a slot*.
    ///
    /// The default is right for every dispatcher that does not queue: nothing waits, so the whole
    /// wall clock is run time. Only [`crate::dispatch::RealDispatcher`], which holds a
    /// process-wide concurrency permit, overrides it.
    fn dispatch_ballot_timed(
        &self,
        cli: &AgenticCli,
        task: &CouncilTask,
        ctx: &BallotContext,
    ) -> TimedOutcome {
        let started = std::time::Instant::now();
        let outcome = self.dispatch_ballot_detailed(cli, task, ctx);
        TimedOutcome {
            outcome,
            queued_ms: 0,
            ran_ms: started.elapsed().as_millis() as u64,
        }
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

#[cfg(test)]
mod login_tests {
    use super::*;

    /// Every built-in seat key has a sign-in command, each the CLI's OWN interactive flow —
    /// the platform hosts them in a PTY and never implements provider auth itself.
    #[test]
    fn every_builtin_seat_has_a_default_login_invocation() {
        // Iterated off the REAL registry so a newly added seat without a sign-in command
        // fails here (Copilot, PR#278) — a hardcoded key list can't catch new seats.
        let builtins = crate::registry::builtin();
        assert!(!builtins.is_empty());
        for seat in &builtins {
            assert!(
                seat.login_invocation.is_some() || default_login_invocation(&seat.key).is_some(),
                "built-in seat {} has no sign-in command — add it to default_login_invocation \
                 or the registry entry",
                seat.key
            );
        }
        assert_eq!(default_login_invocation("unknown-seat"), None);
        // The claude entry signs in the WORKER home, never the operator's own config.
        assert!(default_login_invocation("claude")
            .unwrap()
            .contains(".wicked-worker"));
    }
}
