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

/// How to parse NDJSON output and detect turn-end in a persistent PTY session.
/// Each variant matches one CLI's `--output-format` / `--mode` output shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SessionAdapterKind {
    /// Claude `--output-format stream-json --verbose`.
    /// Sentinel: `{"type":"result"}`. Text: `content[].text` deltas.
    #[default]
    ClaudeNdjson,
    /// GitHub Copilot `--output-format json`.
    /// Sentinel: `{"type":"result"}`. Text: `assistant.message_delta.data.deltaContent`.
    CopilotJson,
    /// `pi --mode json`.
    /// Sentinel: `{"type":"turn_end"}`. Text: `message_update.assistantMessageEvent.delta`.
    PiJson,
    /// No structured format: every line is a text delta; turn ends on process exit.
    Passthrough,
}

impl SessionAdapterKind {
    /// The JSON `"type"` field value that signals end-of-turn, or `None` for process-exit-only.
    pub fn result_type(self) -> Option<&'static str> {
        match self {
            Self::ClaudeNdjson | Self::CopilotJson => Some("result"),
            Self::PiJson => Some("turn_end"),
            Self::Passthrough => None,
        }
    }
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
    /// One-shot flags to strip when building a PTY session argv (e.g. `["-p", "--print"]`).
    #[serde(default)]
    pub session_strip_flags: Vec<String>,
    /// Flags to inject when starting a PTY session (e.g. `["--output-format", "json"]`).
    #[serde(default)]
    pub session_inject_flags: Vec<String>,
    /// NDJSON parsing strategy for persistent PTY sessions. `None` = no session support.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_adapter: Option<SessionAdapterKind>,
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

/// Isolated, timeboxed dispatch of the 4-question scaffold to one CLI.
pub trait Dispatcher {
    /// Dispatch the scaffold to one CLI and collect its vote (`None` on failure/timeout).
    fn dispatch(&self, cli: &AgenticCli, task: &CouncilTask) -> Option<Vote>;
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
