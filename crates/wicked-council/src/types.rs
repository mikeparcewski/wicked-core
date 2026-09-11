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
    /// Enable wicked-core's OS write-containment floor for this worker, defaulting to off for
    /// staged rollout. This is a kernel write jail where supported, not an audit trail, read jail,
    /// or exfiltration/DLP control: model egress and non-curated reads remain available.
    #[serde(default)]
    pub os_sandbox: bool,
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
    /// (F-7R2-006, wave 6) The LAUNCHER's health verdict for this seat — the result of its
    /// sign-in / usability probe (wicked-crew's `GET /roster` `auth` + `council_eligible`), carried
    /// on the roster it hands the engine so routing can act on it. `Some(usable: false)` BENCHES
    /// the seat for the run: it is never convened on a council, never picked by the
    /// evaluator≠creator reassignment, never handed a triage or judge session, and
    /// `unitDistributed.degradedReason` names it. `Some(usable: true)` is a seat the launcher
    /// found signed in (or whose CLI declares a no-auth free tier); `None` (the wire default —
    /// a launcher that predates this field, a TOML registry record) means UNKNOWN and the seat
    /// is treated as eligible until it fails authentication in the run. Additive: absent on the
    /// wire when `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<SeatHealth>,
}

/// The launcher's usability verdict for one seat ([`AgenticCli::health`], F-7R2-006).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatHealth {
    /// Whether the seat can take work: its CLI is signed in, or declares a no-auth free tier.
    pub usable: bool,
    /// Why not, when `usable: false` — the launcher's own words (`signed out`, `dispatch budget
    /// exhausted`, …), rendered into `degradedReason`. `None` when usable or unstated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl SeatHealth {
    /// A seat the launcher found usable.
    pub fn usable() -> Self {
        SeatHealth {
            usable: true,
            reason: None,
        }
    }

    /// A seat the launcher benched, with its reason.
    pub fn unusable(reason: impl Into<String>) -> Self {
        SeatHealth {
            usable: false,
            reason: Some(reason.into()),
        }
    }
}

/// Built-in sign-in commands for the known seat keys — used when a registry entry does not
/// override `login_invocation`. Each is the seat's OWN documented interactive flow (device-code
/// or URL+paste), so it works inside a PTY with no localhost-callback assumptions.
///
/// Every command is DERIVED, not a literal: it is prefixed with the seat's RESOLVED configuration
/// root (`wicked_apps_core::spawn::seat_config_for` — the same resolver the ballot spawn, the ACP
/// worker spawn and the wrapped worker set the seat's environment from), so what the studio tells
/// an operator to sign in is exactly the directory the seats run under. F-013: the claude command
/// used to hard-code `$HOME/.wicked-worker/claude`; with `WICKED_WORKER_HOME` pointing elsewhere
/// the operator signed in the wrong directory, the UI said "signed in" and every ballot still
/// exited "Not logged in". core#410 (F-010): the other seats' commands used to carry no root at
/// all, so `codex login` signed in the OPERATOR's `~/.codex` while the seats (now) run under
/// `<worker home>/codex` — the same mismatch, four more times. Under the operator's inherit hatch
/// every seat runs on the operator's own configuration, so the commands are the plain ones. When
/// a seat root cannot be resolved or validated (no home directory, a relative
/// `WICKED_WORKER_HOME`, a planted symlink) there is NO sign-in command — `None`, fail closed,
/// exactly as the spawns then refuse (codex, PR#413: a fallback to a default spelling would send
/// the operator to sign in a directory no seat will run under). Resolved on every call — a roster
/// read after the environment changed reads the environment, not a cached spelling.
#[must_use]
pub fn default_login_invocation(key: &str) -> Option<String> {
    use wicked_apps_core::spawn::{seat_config_for, SeatCli, SeatConfig};
    let (cli, login) = match key {
        // The worker home (crew#267 option 3): sign in the ENGINE-owned config dir, not the
        // operator's — inside the REPL, `/login` runs the URL+paste flow.
        "claude" => (SeatCli::Claude, "claude"),
        "codex" => (SeatCli::Codex, "codex login --device-auth"),
        "copilot" => (SeatCli::Copilot, "copilot login"),
        "opencode" => (SeatCli::Opencode, "opencode auth login"),
        "pi" => (SeatCli::Pi, "pi"),
        // No configuration-home variable is known for agy: it signs in where it runs (the
        // operator's `~/.gemini/…` configuration) — a documented residual of core#410; its seats
        // at least run quiet (`AGY_CLI_HIDE_LOGO` / `AGY_CLI_HIDE_ACCOUNT_INFO`).
        "agy" => return Some("agy".to_string()),
        _ => return None,
    };
    match seat_config_for(cli) {
        Ok(SeatConfig::Inherit) => Some(login.to_string()),
        Ok(cfg @ SeatConfig::Isolated { .. }) => {
            // The sign-in command writes CREDENTIALS into the seat's directories, before any engine
            // spawn has prepared them (Copilot, #426): prepare them here — created private, every
            // owned directory (opencode's `<xdg>/opencode` app dirs included) no-follow checked —
            // and fail CLOSED (no sign-in surface) on a planted link, exactly as the spawns refuse.
            if cfg.ensure_dirs().is_err() {
                return None;
            }
            let SeatConfig::Isolated { set, .. } = &cfg else {
                unreachable!("matched Isolated above");
            };
            let mut out = String::new();
            for (var, dir) in set {
                out.push_str(var);
                out.push('=');
                out.push_str(&shell_double_quote(&dir.display().to_string()));
                out.push(' ');
            }
            out.push_str(login);
            Some(out)
        }
        Err(_) => None,
    }
}

/// Wrap `s` in POSIX double quotes so a path with a space or a `$` survives the studio's sign-in
/// terminal verbatim. Inside double quotes `"`, `$` and `` ` `` are always special; a backslash is
/// special ONLY before one of those, another backslash, or the closing quote — so a Windows path
/// (`C:\Users\op\...`) passes through unchanged rather than doubled. The sign-in commands are
/// POSIX-shell spellings already (`$HOME` in the old literal) — this keeps that contract.
fn shell_double_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        let escape = match c {
            '"' | '$' | '`' => true,
            '\\' => matches!(chars.peek(), Some('"' | '$' | '`' | '\\') | None),
            _ => false,
        };
        if escape {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
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

/// WHY a seat failed, read off the CLI's own words — the classification an operator acts on.
///
/// [`SeatFailureKind`] names the dispatch BRANCH (the process exited non-zero); this names the
/// CAUSE when the output matches a signature the engine knows the fix for. `None` on the record
/// means nothing recognisable was said, not that nothing went wrong. Serialized snake_case; rides
/// the `councilSeatFailed` event as `reason`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatFailureReason {
    /// The seat's configuration directory holds no login, so the CLI refused before doing any
    /// work (`Not logged in · Please run /login`, `Authentication required`, `not
    /// authenticated`, …). The fix is a sign-in of THAT directory — the roster's
    /// `login_invocation` names it. F-030/F-031: this was every claude ballot on a fresh install,
    /// recorded as a bare `non_zero_exit` with an empty stderr.
    NotLoggedIn,
}

impl SeatFailureReason {
    /// Stable snake_case token for events and degrade strings.
    pub fn as_str(self) -> &'static str {
        match self {
            SeatFailureReason::NotLoggedIn => "not_logged_in",
        }
    }

    /// Classify a failed seat's output. ASCII-case-insensitive substring match over BOTH streams —
    /// claude prints its refusal on STDOUT (`Not logged in · Please run /login`, exit 1, stderr
    /// empty), other CLIs on stderr. Deliberately narrow: an unrecognised failure stays
    /// unclassified rather than mislabelled. Scans each stream in place — no combined or
    /// lowercased copy — because it runs over the UNTRUNCATED output of `wait_with_output`, which
    /// a pathological seat can make large (Copilot, PR#413).
    pub fn classify(stdout: &str, stderr: &str) -> Option<Self> {
        const NOT_LOGGED_IN: &[&str] = &[
            "not logged in",
            "run /login",
            "authentication required",
            "authentication_error",
            "not authenticated",
            "unauthenticated",
            "please log in",
            "please login",
            "please sign in",
            "login required",
            "not signed in",
        ];
        [stdout, stderr]
            .iter()
            .any(|stream| {
                NOT_LOGGED_IN
                    .iter()
                    .any(|sig| contains_ignore_ascii_case(stream, sig))
            })
            .then_some(SeatFailureReason::NotLoggedIn)
    }
}

/// `haystack.to_lowercase().contains(needle)` for an ASCII-lowercase `needle`, without the copy:
/// a byte-window scan with `eq_ignore_ascii_case`. Non-ASCII bytes in the haystack never equal an
/// ASCII needle byte, so UTF-8 multi-byte sequences simply fail to match — no boundary handling
/// needed.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    if n.is_empty() {
        return true;
    }
    if h.len() < n.len() {
        return false;
    }
    h.windows(n.len()).any(|w| w.eq_ignore_ascii_case(n))
}

/// The captured diagnostics for one seat that failed to vote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatFailure {
    /// Which of the dispatch branches was taken.
    pub kind: SeatFailureKind,
    /// The process exit code, when the process ran to completion.
    pub exit_code: Option<i32>,
    /// Captured stderr within [`STDERR_CAPTURE_LIMIT`]: the whole text when it fits, else its HEAD
    /// and TAIL around an elision marker — a usage message or a stack trace's head, AND the final
    /// error line (codex, PR#413: a head-only cut lost a `Not logged in` printed last). `run_in_isolation`
    /// already piped stderr and then dropped it on the floor; this is that artifact, kept.
    pub stderr: String,
    /// The TAIL of captured stdout, truncated to [`STDERR_CAPTURE_LIMIT`] — kept only when the seat
    /// FAILED (a vote is parsed from stdout, never stored here). F-031: claude prints `Not logged
    /// in · Please run /login` on STDOUT and exits 1 with stderr EMPTY, so a record keeping stderr
    /// alone said nothing about why. The tail rather than the head: a CLI states its final error
    /// last. `#[serde(default)]`: records persisted before this field read as empty.
    #[serde(default)]
    pub stdout: String,
    /// The OS/IO error text, where the branch has one.
    pub detail: String,
    /// The classified cause, when the seat's own words match a known signature. Judged over the
    /// UNTRUNCATED streams as they are attached ([`Self::with_output`] over both at once;
    /// [`Self::with_stdout`] / [`Self::with_stderr`] over the full text given plus whatever the
    /// other field holds, never downgrading an earlier positive) — so a signature past the storage
    /// cap still classifies even where the stored text no longer shows it. Never set by hand.
    #[serde(default)]
    pub reason: Option<SeatFailureReason>,
}

/// Cap on retained stderr per seat. Enough to carry a usage message or a stack trace's head,
/// bounded so a runaway CLI cannot balloon an event payload or the task record.
pub const STDERR_CAPTURE_LIMIT: usize = 4096;

/// The marker stored between the kept head and tail of an over-cap stderr.
const ELISION_MARKER: &str = "\n…[truncated]…\n";

/// The last [`STDERR_CAPTURE_LIMIT`] bytes of `s`, cut forward to a char boundary (whole
/// characters only; at most one partial character dropped from the front).
fn tail_within_cap(s: &str) -> String {
    if s.len() <= STDERR_CAPTURE_LIMIT {
        return s.to_string();
    }
    let mut start = s.len() - STDERR_CAPTURE_LIMIT;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].to_string()
}

/// `s` whole when it fits [`STDERR_CAPTURE_LIMIT`]; else its head and tail around
/// [`ELISION_MARKER`], the two halves cut on char boundaries so the total stays valid UTF-8 and
/// within the cap.
fn head_and_tail_within_cap(s: &str) -> String {
    if s.len() <= STDERR_CAPTURE_LIMIT {
        return s.to_string();
    }
    let budget = STDERR_CAPTURE_LIMIT - ELISION_MARKER.len();
    // Head: walk BACK to a boundary (never past the budget).
    let mut head_end = budget / 2;
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    // Tail: walk FORWARD to a boundary (never past the budget).
    let mut tail_start = s.len() - (budget - head_end);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    let mut out = String::with_capacity(STDERR_CAPTURE_LIMIT);
    out.push_str(&s[..head_end]);
    out.push_str(ELISION_MARKER);
    out.push_str(&s[tail_start..]);
    out
}

impl SeatFailure {
    /// A failure with no captured process output — the pre-spawn branches.
    pub fn new(kind: SeatFailureKind, detail: impl Into<String>) -> Self {
        SeatFailure {
            kind,
            exit_code: None,
            stderr: String::new(),
            stdout: String::new(),
            detail: detail.into(),
            reason: None,
        }
    }

    /// Attach BOTH captured streams of a completed process: classified over the full text of each
    /// FIRST (codex, PR#413: classifying after the cut missed a signature past 4096 bytes), then
    /// stored within the cap — stderr as head+tail, stdout as tail.
    pub fn with_output(mut self, stdout: &str, stderr: &str) -> Self {
        self.reason = SeatFailureReason::classify(stdout, stderr);
        self.stdout = tail_within_cap(stdout);
        self.stderr = head_and_tail_within_cap(stderr);
        self
    }

    /// Attach captured stderr: classified over its FULL text (with whatever stdout is already
    /// attached; an earlier positive classification is kept), then stored as head+tail within
    /// [`STDERR_CAPTURE_LIMIT`], cut on char boundaries so the result stays valid UTF-8.
    pub fn with_stderr(mut self, stderr: &str) -> Self {
        self.reason = SeatFailureReason::classify(&self.stdout, stderr).or(self.reason);
        self.stderr = head_and_tail_within_cap(stderr);
        self
    }

    /// Attach captured stdout: classified over its FULL text (with whatever stderr is already
    /// attached; an earlier positive classification is kept), then stored as its TAIL within
    /// [`STDERR_CAPTURE_LIMIT`] — a CLI states its final error last.
    pub fn with_stdout(mut self, stdout: &str) -> Self {
        self.reason = SeatFailureReason::classify(stdout, &self.stderr).or(self.reason);
        self.stdout = tail_within_cap(stdout);
        self
    }

    /// One-line summary suitable for a degrade string: the branch, the exit code, the classified
    /// cause when there is one, plus the most specific evidence available.
    pub fn summary(&self) -> String {
        let mut s = self.kind.as_str().to_string();
        if let Some(code) = self.exit_code {
            s.push_str(&format!(" (exit {code})"));
        }
        if let Some(reason) = self.reason {
            s.push_str(&format!(" [{}]", reason.as_str()));
        }
        // stderr is the more specific artifact when both are present — it is the CLI's own words;
        // stdout is next (F-031: claude's refusal lives there); the OS error text is last.
        let evidence = if !self.stderr.is_empty() {
            self.stderr.trim()
        } else if !self.stdout.is_empty() {
            self.stdout.trim()
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
        // READ side of the crate-wide env lock: the claude sign-in command below RESOLVES
        // `WICKED_WORKER_HOME`, which the env-mutating tests in this binary pin to fixtures.
        let _env = crate::test_env::ENV_LOCK
            .read()
            .unwrap_or_else(|p| p.into_inner());
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
        // The claude entry signs in the WORKER home — the resolved seat dir, never the operator's
        // own config. (Under the operator's inherit hatch the seats DO run on the operator's
        // config, and the sign-in is plain `claude`; a host with the hatch set is a supported
        // configuration, not a failure of this test.)
        let claude = default_login_invocation("claude").unwrap_or_else(|| {
            panic!(
                "claude has no sign-in command on this host — the seat dir did not resolve: {:?}",
                wicked_apps_core::spawn::seat_claude_config_dir()
            )
        });
        match wicked_apps_core::spawn::seat_claude_config_dir() {
            None => assert_eq!(claude, "claude"),
            Some(dir) => {
                let dir = dir.expect("this process has a home directory");
                assert_eq!(
                    claude,
                    format!(
                        "CLAUDE_CONFIG_DIR={} claude",
                        shell_double_quote(&dir.display().to_string())
                    ),
                    "the sign-in names EXACTLY the directory the seats run under (F-013)"
                );
                assert!(claude.starts_with("CLAUDE_CONFIG_DIR=\""), "{claude}");
                assert!(claude.ends_with("claude\" claude"), "{claude}");
                assert!(
                    !claude.contains("$HOME"),
                    "resolved, not the default spelling"
                );
            }
        }
    }

    /// codex, PR#413: when the seat dir cannot be resolved there is NO sign-in command — the ballot
    /// refuses to spawn on that value, so a fallback spelling would send the operator to sign in a
    /// directory no seat runs under. A relative `WICKED_WORKER_HOME` is the reproducible case.
    #[test]
    fn no_sign_in_command_when_the_seat_dir_is_unresolvable() {
        if wicked_apps_core::spawn::inherits_operator_config() {
            // Under the hatch the sign-in is plain `claude` regardless of the worker home.
            return;
        }
        // WRITE side of the crate-wide env lock: this test MUTATES `WICKED_WORKER_HOME`, which the
        // dispatch module's tests also pin (Copilot, PR#413: one lock, not one per module).
        let _g = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let prior = std::env::var_os(wicked_apps_core::spawn::WORKER_HOME_ENV);
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, "relative/worker");
        let login = default_login_invocation("claude");
        // core#410: read under the SAME unresolvable override — the other seats resolve through
        // the same base and must fail closed the same way; agy has no root and is unaffected.
        let codex = default_login_invocation("codex");
        let agy = default_login_invocation("agy");
        match &prior {
            Some(v) => std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, v),
            None => std::env::remove_var(wicked_apps_core::spawn::WORKER_HOME_ENV),
        }
        assert_eq!(
            login, None,
            "fail closed: no sign-in surface for an unresolvable seat dir"
        );
        assert_eq!(
            codex, None,
            "codex signs in ITS seat root, which is unresolvable here too"
        );
        assert_eq!(agy.as_deref(), Some("agy"));
    }

    /// core#410 (F-010): every seat's sign-in command names the SEAT ROOT the spawns run under —
    /// the CLI's own configuration-home variable(s), the same resolver, the same base — so the
    /// operator signs in the directory a ballot/worker/chat seat will actually read credentials
    /// from. Without this the studio's Sign-in signed in `~/.codex` while the seat ran under
    /// `<worker home>/codex`, and the roster read the operator's login as the seat's.
    #[test]
    fn every_seats_sign_in_command_names_its_own_root_under_the_worker_home() {
        if wicked_apps_core::spawn::inherits_operator_config() {
            for (key, plain) in [
                ("codex", "codex login --device-auth"),
                ("pi", "pi"),
                ("copilot", "copilot login"),
                ("opencode", "opencode auth login"),
            ] {
                assert_eq!(default_login_invocation(key).as_deref(), Some(plain));
            }
            return;
        }
        let _g = crate::test_env::ENV_LOCK
            .write()
            .unwrap_or_else(|p| p.into_inner());
        let base = std::env::temp_dir().join(format!("wc-login-roots-{}", std::process::id()));
        let prior = std::env::var_os(wicked_apps_core::spawn::WORKER_HOME_ENV);
        std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &base);
        let got: Vec<(&str, Option<String>)> = ["codex", "pi", "copilot", "opencode", "claude"]
            .into_iter()
            .map(|k| (k, default_login_invocation(k)))
            .collect();
        match &prior {
            Some(v) => std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, v),
            None => std::env::remove_var(wicked_apps_core::spawn::WORKER_HOME_ENV),
        }
        let q = |p: std::path::PathBuf| shell_double_quote(&p.display().to_string());
        let expect = |key: &str| got.iter().find(|(k, _)| *k == key).unwrap().1.clone();
        assert_eq!(
            expect("codex"),
            Some(format!(
                "CODEX_HOME={} codex login --device-auth",
                q(base.join("codex"))
            ))
        );
        assert_eq!(
            expect("pi"),
            Some(format!("PI_CODING_AGENT_DIR={} pi", q(base.join("pi"))))
        );
        assert_eq!(
            expect("copilot"),
            Some(format!(
                "COPILOT_HOME={} copilot login",
                q(base.join("copilot"))
            ))
        );
        assert_eq!(
            expect("opencode"),
            Some(format!(
                "XDG_CONFIG_HOME={} XDG_DATA_HOME={} XDG_STATE_HOME={} opencode auth login",
                q(base.join("opencode").join("config")),
                q(base.join("opencode").join("data")),
                q(base.join("opencode").join("state"))
            ))
        );
        assert_eq!(
            expect("claude"),
            Some(format!(
                "CLAUDE_CONFIG_DIR={} claude",
                q(base.join("claude"))
            )),
            "the claude spelling is unchanged by the generalisation"
        );
        // Copilot, #426: the directories the sign-in command will write credentials into are
        // prepared (private) by the roster read itself — opencode's app dirs included.
        for d in [
            base.join("codex"),
            base.join("pi"),
            base.join("copilot"),
            base.join("opencode").join("data").join("opencode"),
        ] {
            assert!(d.is_dir(), "{} is prepared before sign-in", d.display());
        }
        #[cfg(unix)]
        {
            // A planted link where a seat root should be: NO sign-in command (fail closed), never
            // a command that would write the operator's credentials through the link.
            let base2 = std::env::temp_dir().join(format!("wc-login-link-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base2);
            std::fs::create_dir_all(&base2).unwrap();
            let operator = base2.join("operator-pi");
            std::fs::create_dir_all(&operator).unwrap();
            std::os::unix::fs::symlink(&operator, base2.join("pi")).unwrap();
            std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, &base2);
            let pi = default_login_invocation("pi");
            match &prior {
                Some(v) => std::env::set_var(wicked_apps_core::spawn::WORKER_HOME_ENV, v),
                None => std::env::remove_var(wicked_apps_core::spawn::WORKER_HOME_ENV),
            }
            assert_eq!(
                pi, None,
                "a planted seat-root link yields no sign-in command"
            );
            let _ = std::fs::remove_dir_all(&base2);
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    /// A path with shell-special characters survives the sign-in terminal verbatim; a Windows
    /// path's backslashes (not special before an ordinary character) pass through unchanged.
    #[test]
    fn the_sign_in_path_is_double_quoted_with_the_specials_escaped() {
        assert_eq!(shell_double_quote("/plain/path"), r#""/plain/path""#);
        assert_eq!(
            shell_double_quote(r#"/with space/$var/"q"/`tick`"#),
            r#""/with space/\$var/\"q\"/\`tick\`""#
        );
        assert_eq!(
            shell_double_quote(r"C:\Users\op\.wicked-worker\claude"),
            r#""C:\Users\op\.wicked-worker\claude""#,
            "a lone backslash before an ordinary char is literal inside double quotes"
        );
        // A RUN of backslashes before an ordinary char: only the ones followed by another backslash
        // are escaped — the shell collapses `\\` to `\` and keeps the final `\b` verbatim, so
        // `"a\\\b"` evaluates to `a\\b` (the minimal correct form, not `\\\\`).
        assert_eq!(shell_double_quote(r"a\\b"), r#""a\\\b""#);
        assert_eq!(shell_double_quote(r"a\$b"), r#""a\\\$b""#);
        assert_eq!(shell_double_quote(r"trailing\"), r#""trailing\\""#);
    }
}

#[cfg(test)]
mod failure_reason_tests {
    use super::*;

    /// F-031: claude's refusal is on STDOUT with stderr empty — the classification must read both
    /// streams, and the record must keep the stdout tail so the event says WHY.
    #[test]
    fn a_not_logged_in_refusal_on_stdout_is_classified_and_kept() {
        let f = SeatFailure {
            kind: SeatFailureKind::NonZeroExit,
            exit_code: Some(1),
            stderr: String::new(),
            stdout: String::new(),
            detail: String::new(),
            reason: None,
        }
        .with_stderr("")
        .with_stdout("Not logged in · Please run /login\n");
        assert_eq!(f.reason, Some(SeatFailureReason::NotLoggedIn));
        assert_eq!(f.reason.unwrap().as_str(), "not_logged_in");
        assert!(f.stdout.contains("Not logged in"), "{f:?}");
        let summary = f.summary();
        assert!(
            summary.contains("non_zero_exit (exit 1) [not_logged_in]"),
            "{summary}"
        );
        assert!(
            summary.contains("Not logged in · Please run /login"),
            "{summary}"
        );
    }

    /// The same refusal on stderr (other CLIs) classifies too; the order the streams are attached
    /// in does not matter, because each attach re-classifies over both.
    #[test]
    fn classification_reads_either_stream_in_either_order() {
        let f = SeatFailure::new(SeatFailureKind::NonZeroExit, "")
            .with_stdout("")
            .with_stderr("error: Authentication required — run `codex login`");
        assert_eq!(f.reason, Some(SeatFailureReason::NotLoggedIn));
        assert_eq!(
            SeatFailureReason::classify("NOT LOGGED IN", ""),
            Some(SeatFailureReason::NotLoggedIn),
            "case-insensitive"
        );
    }

    /// An unrecognised failure stays UNCLASSIFIED — never mislabelled as a login problem.
    #[test]
    fn an_unrelated_failure_is_not_classified() {
        let f = SeatFailure::new(SeatFailureKind::NonZeroExit, "")
            .with_stderr("agy: unknown flag --headless")
            .with_stdout("usage: agy [options]");
        assert_eq!(f.reason, None);
        assert!(!f.summary().contains('['), "{}", f.summary());
        assert_eq!(SeatFailureReason::classify("", ""), None);
    }

    /// stdout keeps the TAIL (a CLI states its final error last), cut on a char boundary.
    #[test]
    fn stdout_keeps_the_tail_within_the_cap_on_a_char_boundary() {
        let head = "x".repeat(STDERR_CAPTURE_LIMIT * 2);
        let out = format!("{head}éé Not logged in");
        let f = SeatFailure::new(SeatFailureKind::NonZeroExit, "").with_stdout(&out);
        assert!(f.stdout.len() <= STDERR_CAPTURE_LIMIT, "{}", f.stdout.len());
        assert!(f.stdout.ends_with("éé Not logged in"), "the tail survives");
        assert_eq!(f.reason, Some(SeatFailureReason::NotLoggedIn));
        // A multi-byte char straddling the cut is dropped whole, never split.
        let straddle = format!("{}é{}", "a".repeat(STDERR_CAPTURE_LIMIT + 1), "b");
        let cut = SeatFailure::new(SeatFailureKind::NonZeroExit, "").with_stdout(&straddle);
        assert!(std::str::from_utf8(cut.stdout.as_bytes()).is_ok());
        assert!(cut.stdout.len() <= STDERR_CAPTURE_LIMIT);
    }

    /// codex, PR#413: an auth signature BEYOND the 4096-byte storage cap must still classify, and
    /// the stored stderr must still SHOW it (head + tail, not head only). Both streams.
    #[test]
    fn an_auth_signature_beyond_the_cap_is_classified_and_the_tail_is_kept() {
        let noise = "x".repeat(STDERR_CAPTURE_LIMIT + 500);
        let stderr =
            format!("usage: seat [options]\n{noise}\nerror: Not logged in · Please run /login");
        let f = SeatFailure::new(SeatFailureKind::NonZeroExit, "").with_output("", &stderr);
        assert_eq!(
            f.reason,
            Some(SeatFailureReason::NotLoggedIn),
            "classified past the cap"
        );
        assert!(f.stderr.len() <= STDERR_CAPTURE_LIMIT, "{}", f.stderr.len());
        assert!(
            f.stderr.starts_with("usage: seat [options]"),
            "the head survives"
        );
        assert!(
            f.stderr
                .ends_with("error: Not logged in · Please run /login"),
            "the tail survives: {:?}",
            &f.stderr[f.stderr.len().saturating_sub(80)..]
        );
        assert!(f.stderr.contains("…[truncated]…"), "the cut is visible");
        // The same through the single-stream setter.
        let g = SeatFailure::new(SeatFailureKind::NonZeroExit, "").with_stderr(&stderr);
        assert_eq!(g.reason, Some(SeatFailureReason::NotLoggedIn));
        assert!(g.stderr.ends_with("Please run /login"));
        // A signature at the HEAD of an over-cap stdout: the stored tail no longer shows it, but
        // the classification — judged over the full text — does.
        let stdout = format!("Not logged in\n{noise}\n{noise}");
        let h = SeatFailure::new(SeatFailureKind::NonZeroExit, "").with_output(&stdout, "");
        assert_eq!(h.reason, Some(SeatFailureReason::NotLoggedIn));
        assert!(h.stdout.len() <= STDERR_CAPTURE_LIMIT);
        // Attaching the other (empty) stream afterwards never downgrades the classification.
        let h = h.with_stderr("");
        assert_eq!(h.reason, Some(SeatFailureReason::NotLoggedIn));
        // Head+tail cuts land on char boundaries.
        let wide = "é".repeat(STDERR_CAPTURE_LIMIT);
        let w = SeatFailure::new(SeatFailureKind::NonZeroExit, "").with_stderr(&wide);
        assert!(w.stderr.len() <= STDERR_CAPTURE_LIMIT);
        assert!(w.stderr.starts_with('é') && w.stderr.ends_with('é'));
    }

    /// Records persisted before `stdout`/`reason` existed still read (both default).
    #[test]
    fn a_record_without_the_new_fields_deserializes() {
        let legacy = r#"{"kind":"non_zero_exit","exit_code":1,"stderr":"","detail":""}"#;
        let f: SeatFailure = serde_json::from_str(legacy).unwrap();
        assert_eq!(f.stdout, "");
        assert_eq!(f.reason, None);
        let round: SeatFailure =
            serde_json::from_str(&serde_json::to_string(&f.with_stdout("Not logged in")).unwrap())
                .unwrap();
        assert_eq!(round.reason, Some(SeatFailureReason::NotLoggedIn));
    }
}
