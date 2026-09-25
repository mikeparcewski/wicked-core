//! Real-time teaming (DES-TEAMING-001, #590). Per DES §9 every seam's engine piece lives here.
//!
//! **S2 — the monitor subscription (#601, DES §4).** A teamed unit's ACP carrier emits
//! [`CoreEvent::UnitCheckpoint`] at each terminal tool call ([`TeamTurn::observe`]). The
//! per-daemon supervisor ([`spawn_supervisor`]) subscribes to the engine's event fan-out, and at
//! most once per [`BATCH_MIN_INTERVAL`] per monitor it snapshots the worktree, diffs the snapshot
//! against the previous one (the unit's dispatch baseline for the first batch) and hands the
//! incremental diff to a warm, READ-ONLY monitor session on a seat instance distinct from the
//! creator (`claude#2`). Every `FINDING` a monitor replies with is confirmed MECHANICALLY against
//! the snapshot tree (the quoted line must BE line `line` of `path`, exactly), held to the bar
//! (`high` | `medium`), deduplicated by line TEXT, and only then emitted as
//! [`CoreEvent::MonitorFinding`]. When the worker's turn ends, [`final_pass`] reviews the settled
//! tree once more, re-confirms every finding and returns the [`TeamLedger`]. The monitor count is
//! a plain [`TeamPlan`] parameter until S4's policy lands, and the candidates arrive the same way
//! until S5's `RoutingInfo::Teamed` does.
//!
//! **S3 — monitor→worker injection (#602, DES §5).** The steer mailbox the supervisor writes HIGH
//! findings into, the advice text a steer carries, the per-attempt delivery record, the
//! carrier-independent "not delivered mid-turn" disclosure, and the worker's
//! `ADVICE <id>: ACCEPT|DECLINE — <reason>` parsing. The one mid-turn carrier is the ACP adapter's
//! `_session/steering` (claude-agent-acp 0.73.0, `dist/acp-agent.js:1186-1272`). Every steer
//! carries `idleBehavior: "promptRequired"`, so a steer that lands after the turn settled returns
//! `promptRequired` instead of starting a detached turn (`acp-agent.js:1228-1242`). No other
//! carrier takes a steer, and none gets a second mechanism (DES §5.1): whatever is still queued
//! when an attempt's turn ends is recorded as not delivered mid-turn, with the reason, and goes to
//! the gate.
//!
//! [`Finding`] is THE finding type (DES §7 `monitorFinding`): S2 constructs it, the ledger embeds
//! it, S3 steers it. Advisory throughout: nothing here denies, injects a verdict or decides. S6
//! (#603) carries the ledger to the gate.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::event::CoreEvent;

/// A closed wire token set: one variant per token, serialized as exactly that token, and an
/// unknown token refused at parse (a typed field never travels as a free string).
macro_rules! wire_enum {
    ($(#[$m:meta])* $vis:vis enum $name:ident { $($(#[$vm:meta])* $var:ident = $tok:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        $vis enum $name {
            $($(#[$vm])* #[serde(rename = $tok)] $var,)+
        }

        impl $name {
            /// The wire token.
            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$var => $tok,)+
                }
            }
        }
    };
}

pub mod events;
pub mod publish;

// ── Constants (DES §4.8; env-overridable for rigs only) ──────────────────────────────────────────

/// A monitor starts at most one batch per this interval.
pub const BATCH_MIN_INTERVAL: Duration = Duration::from_secs(60);
/// Batches per monitor per attempt — a hard cost ceiling; exhausting it is disclosed.
pub const MAX_BATCHES: u32 = 10;
/// Diff bytes per batch prompt; past it the prompt carries the name-status list and says so.
pub const DIFF_CAP: usize = 48 * 1024;
/// One monitor turn's wall budget; a turn over it loses its batch (disclosed).
pub const MONITOR_TURN_BUDGET: Duration = Duration::from_secs(240);
/// The final pass's wall budget: the gate never waits unboundedly on advice.
pub const FINAL_PASS_BUDGET: Duration = Duration::from_secs(300);

const TITLE_CAP: usize = 256;
const PATHS_CAP: usize = 16;
const TITLES_PER_BATCH: usize = 20;
const EVIDENCE_CAP: usize = 512;
const CLAIM_CAP: usize = 2048;

/// The ACP ToolKinds that cannot change the tree. Every other kind — `edit`, `delete`, `move`,
/// `execute`, `other`, and any kind this list does not name — marks the unit's tree as possibly
/// changed (DES §4.3), so an unknown kind errs toward a review, never toward silence.
const READ_ONLY_KINDS: &[&str] = &["read", "search", "fetch", "think"];

/// The batching and budget limits. [`TeamLimits::from_env`] is the production reading; tests
/// build their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TeamLimits {
    pub batch_min_interval: Duration,
    pub max_batches: u32,
    pub diff_cap: usize,
    pub monitor_turn_budget: Duration,
    pub final_pass_budget: Duration,
}

impl Default for TeamLimits {
    fn default() -> Self {
        Self {
            batch_min_interval: BATCH_MIN_INTERVAL,
            max_batches: MAX_BATCHES,
            diff_cap: DIFF_CAP,
            monitor_turn_budget: MONITOR_TURN_BUDGET,
            final_pass_budget: FINAL_PASS_BUDGET,
        }
    }
}

impl TeamLimits {
    /// The defaults, each overridable by an env variable for rigs (`WICKED_TEAM_*`). An
    /// unparseable value keeps the default.
    pub fn from_env() -> Self {
        let secs = |var: &str, dflt: Duration| {
            std::env::var(var)
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
                .map_or(dflt, Duration::from_secs)
        };
        let d = Self::default();
        Self {
            batch_min_interval: secs("WICKED_TEAM_BATCH_MIN_INTERVAL_SECS", d.batch_min_interval),
            max_batches: std::env::var("WICKED_TEAM_MAX_BATCHES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.max_batches),
            diff_cap: std::env::var("WICKED_TEAM_DIFF_CAP_BYTES")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(d.diff_cap),
            monitor_turn_budget: secs("WICKED_TEAM_MONITOR_TURN_SECS", d.monitor_turn_budget),
            final_pass_budget: secs("WICKED_TEAM_FINAL_PASS_SECS", d.final_pass_budget),
        }
    }
}

/// How many monitors a unit gets and on which seat instances (DES §8). A PLAIN PARAMETER until
/// S4 (`review_plan(..).monitors`) and S5 (`RoutingInfo::Teamed` candidates) land; their read
/// sites replace the caller that builds this, not this module.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TeamPlan {
    /// The target monitor count.
    pub monitors: u8,
    /// Ordered monitor-candidate seat instances (`["claude#2", "claude#3"]`); the first
    /// `monitors` that pass admission are summoned.
    pub candidates: Vec<String>,
}

/// `s` clamped to `max` bytes at a UTF-8 boundary.
pub fn cap_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

// ── Checkpoints (DES §4.2) ────────────────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct ToolMemo {
    kind: Option<String>,
    title: Option<String>,
    paths: Vec<String>,
}

/// The team context of ONE unit turn on the ACP carrier — the `team: Option<&TeamTurn>`
/// parameter of `exec_turn_acp_posture` (DES §4.2, §5.2). S2 owns the tool-call memo and the
/// checkpoint sequence; S3 owns the steer mailbox handle and the re-confirmation root. `None` for
/// chat turns, monitor turns, tests that do not exercise teaming, and the engine's OWN sessions
/// (the agent judge, triage): they share the unit's `(run, ord, attempt)` but are not the worker,
/// so they must never drain its advice, answer it, nor checkpoint for it.
pub struct TeamTurn {
    /// `(run, ord, attempt)`.
    pub key: UnitKey,
    /// S3: the mailbox the supervisor's HIGH advice waits in for this turn's next boundary.
    pub mailbox: SteerMailbox,
    /// S3: the unit's worktree and the git dir its baseline was snapshotted through — what a fresh
    /// snapshot re-confirms a finding against before it is sent. `None` when the unit has no
    /// worktree baseline: nothing can be re-confirmed, so nothing is sent.
    pub confirm_root: Option<(PathBuf, PathBuf)>,
    /// S2: whether the supervisor was attached for this attempt (a team plan named monitors).
    /// Only a TEAMED turn emits `unitCheckpoint` (DES §4.2); every unit turn carries its mailbox.
    teamed: bool,
    /// S2: the unit's worktree — a location under it is reported repo-relative.
    workdir: Option<PathBuf>,
    memo: Mutex<HashMap<String, ToolMemo>>,
    seq: AtomicU64,
}

impl TeamTurn {
    pub(crate) fn new(
        key: UnitKey,
        mailbox: SteerMailbox,
        confirm_root: Option<(PathBuf, PathBuf)>,
        workdir: Option<PathBuf>,
        teamed: bool,
    ) -> Self {
        Self {
            key,
            mailbox,
            confirm_root,
            teamed,
            workdir,
            memo: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// The unit's team context from its [`crate::workflow::StepInput`]; `teamed` says whether the
    /// supervisor holds an `Attach` for this attempt. `None` for the engine's own sessions.
    pub(crate) fn for_unit(
        input: &crate::workflow::StepInput,
        mailbox: &SteerMailbox,
        teamed: bool,
    ) -> Option<Self> {
        if crate::execute_wrapped::is_engine_internal(&input.unit) {
            return None;
        }
        let confirm_root = input.workdir.clone().and_then(|wt| {
            input
                .unit
                .worktree_baseline
                .as_ref()
                .and_then(|b| b.git_dir.clone())
                .map(|gd| (wt, PathBuf::from(gd)))
        });
        Some(Self::new(
            (input.run_id.clone(), input.unit.ord, input.attempt),
            mailbox.clone(),
            confirm_root,
            input.workdir.clone(),
            teamed,
        ))
    }

    fn relative(&self, p: &str) -> String {
        if let Some(root) = self.workdir.as_deref() {
            if let Ok(rel) = Path::new(p).strip_prefix(root) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
        p.to_string()
    }

    /// Observe one agent `session/update` frame of a TEAMED turn (an unteamed one yields `None`
    /// for every frame). `kind`, `title` and `locations` are REMEMBERED per `toolCallId` from the
    /// `tool_call` and refinement frames, because the terminal `tool_call_update` does not repeat
    /// them; that terminal frame (`status` `completed` | `failed`) yields exactly one
    /// [`CoreEvent::UnitCheckpoint`]. Every other frame — message chunks, usage, a non-terminal
    /// update — yields `None`: checkpoints are tool-call boundaries, never tokens.
    pub fn observe(&self, frame: &Value) -> Option<CoreEvent> {
        if !self.teamed {
            return None;
        }
        let update = &frame["params"]["update"];
        let which = update["sessionUpdate"].as_str()?;
        if which != "tool_call" && which != "tool_call_update" {
            return None;
        }
        let id = update["toolCallId"].as_str()?.to_string();
        let mut memo = self.memo.lock().unwrap_or_else(|p| p.into_inner());
        let entry = memo.entry(id.clone()).or_default();
        if let Some(k) = update["kind"].as_str() {
            entry.kind = Some(k.to_string());
        }
        if let Some(t) = update["title"].as_str() {
            entry.title = Some(t.to_string());
        }
        if let Some(locs) = update["locations"].as_array() {
            let paths: Vec<String> = locs
                .iter()
                .filter_map(|l| l["path"].as_str())
                .map(|p| self.relative(p))
                .take(PATHS_CAP)
                .collect();
            if !paths.is_empty() {
                entry.paths = paths;
            }
        }
        let status = update["status"].as_str();
        if which != "tool_call_update" || !matches!(status, Some("completed" | "failed")) {
            return None;
        }
        let done = memo.remove(&id).unwrap_or_default();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        Some(CoreEvent::UnitCheckpoint {
            session: self.key.0.clone(),
            ord: self.key.1,
            attempt: self.key.2,
            seq,
            tool_call_id: id,
            kind: done.kind.unwrap_or_else(|| "other".to_string()),
            title: cap_utf8(done.title.as_deref().unwrap_or(""), TITLE_CAP),
            status: status.unwrap_or_default().to_string(),
            paths: done.paths,
        })
    }
}

/// Whether a checkpoint of `kind` may have changed the tree (DES §4.3).
pub fn kind_may_change_tree(kind: &str) -> bool {
    !READ_ONLY_KINDS.contains(&kind)
}

// ── Findings: parse, bar, confirm, dedup (DES §4.5-4.6) ───────────────────────────────────────────

/// One parsed `FINDING` line, before the bar and the confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawFinding {
    pub severity: String,
    pub path: String,
    pub line: u32,
    pub evidence: String,
    pub claim: String,
    pub suggestion: Option<String>,
}

/// The ledger's `rejected` counters: every `FINDING` line that did not surface, by the first
/// check it failed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rejected {
    pub malformed: u32,
    pub below_bar: u32,
    pub unconfirmed: u32,
    pub duplicate: u32,
}

impl Rejected {
    fn add(&mut self, o: Rejected) {
        self.malformed += o.malformed;
        self.below_bar += o.below_bar;
        self.unconfirmed += o.unconfirmed;
        self.duplicate += o.duplicate;
    }
}

/// A monitor reply's `FINDING` lines, parsed strictly. Returns the parsed findings and the count
/// of `FINDING` lines that were malformed. Every other line — `DONE`, prose — is ignored.
pub fn parse_reply(text: &str) -> (Vec<RawFinding>, u32) {
    let mut out = Vec::new();
    let mut malformed = 0;
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("FINDING ") else {
            continue;
        };
        match parse_finding(rest.trim()) {
            Some(f) => out.push(f),
            None => malformed += 1,
        }
    }
    (out, malformed)
}

fn parse_finding(json: &str) -> Option<RawFinding> {
    let v: Value = serde_json::from_str(json).ok()?;
    let obj = v.as_object()?;
    let text = |k: &str| obj.get(k).and_then(Value::as_str).map(str::to_string);
    let severity = text("severity")?;
    if !matches!(severity.as_str(), "high" | "medium" | "low") {
        return None;
    }
    let path = repo_relative(&text("path")?)?;
    let line = u32::try_from(obj.get("line")?.as_u64()?).ok()?;
    if line == 0 {
        return None;
    }
    let evidence = text("evidence")?;
    let claim = text("claim")?;
    let suggestion = match obj.get("suggestion") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return None,
    };
    Some(RawFinding {
        severity,
        path,
        line,
        evidence,
        claim,
        suggestion,
    })
}

/// `path` as a repo-relative, forward-slash path — `None` for an absolute path, a `..`
/// component, or an empty one.
fn repo_relative(path: &str) -> Option<String> {
    let p = path.replace('\\', "/");
    let p = p.trim_start_matches("./");
    if p.is_empty() || p.starts_with('/') || p.contains(':') {
        return None;
    }
    if p.split('/').any(|seg| seg == ".." || seg.is_empty()) {
        return None;
    }
    Some(p.to_string())
}

/// The finding id: `f-` + the first 16 hex of sha256(`path` ‖ `\n` ‖ normalized `evidence`). Keyed
/// on the line's TEXT, never its number, so an edit that shifts lines mints no new finding.
pub fn finding_id(path: &str, evidence: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(b"\n");
    h.update(normalize_evidence(evidence).as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("f-{hex}")
}

/// Whether line `line` (1-based) of `file` IS `evidence` — the exact text (DES §4.6 step 3,
/// "file:line or it did not happen": the monitor prompt asks for the exact line and
/// `monitorFinding.evidence` is documented as equal to it). Whitespace normalization serves the
/// finding id only ([`finding_id`]), so a re-spaced quote is `unconfirmed`, never emitted as
/// evidence that is not in the tree (codex review of #609). An empty evidence, a line 0 and a
/// missing file confirm nothing.
pub fn confirm(file: Option<&str>, line: u32, evidence: &str) -> bool {
    if evidence.is_empty() {
        return false;
    }
    let Some(i) = line.checked_sub(1) else {
        return false;
    };
    file.and_then(|f| f.lines().nth(i as usize))
        .is_some_and(|l| l == evidence)
}

/// Where `evidence` sits in `file` now: `line` itself when it still matches exactly, else the
/// nearest line with the same NORMALIZED text (DES §4.6 `lineKey`: moved-line correlation, the
/// same key the finding id is on), else `None` (the text is gone — the finding is superseded).
pub fn locate(file: Option<&str>, line: u32, evidence: &str) -> Option<u32> {
    if confirm(file, line, evidence) {
        return Some(line);
    }
    let want = normalize_evidence(evidence);
    if want.is_empty() {
        return None;
    }
    file?
        .lines()
        .enumerate()
        .filter(|(_, l)| normalize_evidence(l) == want)
        .map(|(i, _)| i as u32 + 1)
        .min_by_key(|n| n.abs_diff(line))
}

wire_enum! {
    /// How the attempt's final pass ended (DES-001 §7 `teamLedger.finalPass`; DES-002 §6 #22).
    pub enum FinalPass {
        Completed = "completed",
        TimedOut = "timed_out",
        Skipped = "skipped",
        /// Rows of the attempt are missing from the stream: an incomplete record (DES-002 §4.7).
        StreamGap = "stream_gap",
    }
}

wire_enum! {
    /// `teamLedger.monitors[].status` (and `member.left.status`).
    pub enum MonitorStatus {
        Completed = "completed",
        BudgetExhausted = "budget_exhausted",
        Failed = "failed",
        TimedOut = "timed_out",
    }
}

wire_enum! {
    /// `teamLedger.findings[].delivery`.
    pub enum LedgerDelivery {
        Injected = "injected",
        NotDelivered = "not_delivered",
    }
}

wire_enum! {
    /// `teamLedger.findings[].status`.
    pub enum FindingStatus {
        Accepted = "accepted",
        Declined = "declined",
        Withdrawn = "withdrawn",
        Unanswered = "unanswered",
        Superseded = "superseded",
    }
}

wire_enum! {
    /// `monitorReply.kind`: the monitor's hold-round answer.
    pub enum ReplyKind {
        Hold = "hold",
        Withdraw = "withdraw",
    }
}

wire_enum! {
    /// A council's verdict (`dispute.verdict`, `council.ruled.verdict`). `NoVerdict` spells the
    /// DES token `no_verdict`.
    #[allow(clippy::enum_variant_names)]
    pub enum Verdict {
        Yes = "yes",
        No = "no",
        NoVerdict = "no_verdict",
    }
}

wire_enum! {
    /// Why a council produced no verdict (DES-001 §6.3).
    pub enum NoVerdictReason {
        NoQuorum = "no_quorum",
        SeatsBenched = "seats_benched",
        Error = "error",
        Timeout = "timeout",
        Cap = "cap",
    }
}

/// A monitor's answer on a declined finding at the final pass (S3/S6 fill it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MonitorReply {
    pub kind: ReplyKind,
    pub reason: String,
}

/// A one-off council's verdict on a dispute (S6 fills it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Dispute {
    pub verdict: Verdict,
    pub agreement_pct: Option<u8>,
    pub dissent: Option<u32>,
    pub seats: Vec<String>,
    pub reason: Option<NoVerdictReason>,
}

/// One finding in the ledger (DES §7 `teamLedger.findings[]`). S2 fills everything it owns;
/// `delivery`, `workerReason`, `monitorReply` and `dispute` keep their S2 defaults
/// (`not_delivered` / `null`) until S3 and S6 write them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerFinding {
    /// The finding as it was confirmed and emitted (`monitorFinding`, DES §7), flattened.
    #[serde(flatten)]
    pub finding: Finding,
    pub final_line: Option<u32>,
    /// The SEATS of the other monitors that raised the same finding (they are parties to it).
    pub corroborated_by: Vec<String>,
    pub delivery: LedgerDelivery,
    pub status: FindingStatus,
    pub worker_reason: Option<String>,
    pub monitor_reply: Option<MonitorReply>,
    pub dispute: Option<Dispute>,
}

/// One monitor in the ledger (DES §7 `teamLedger.monitors[]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerMonitor {
    pub monitor_id: String,
    pub seat: String,
    pub batches: u32,
    pub status: MonitorStatus,
    pub error: Option<String>,
}

/// The per-attempt team record the final pass returns (DES §6.1 / §7 `teamLedger`, without the
/// envelope S6 emits it in).
///
/// **Deserialized through [`TeamLedgerWire`]:** `teamPause` is computed, never read from input
/// (a payload claiming `false`, or omitting it, cannot unpause an unresolved HIGH).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "TeamLedgerWire")]
pub struct TeamLedger {
    pub final_pass: FinalPass,
    /// S6 sets it when the ledger is rendered into the judge's WORK payload.
    pub rendered_to_judge: bool,
    pub monitors: Vec<LedgerMonitor>,
    pub findings: Vec<LedgerFinding>,
    pub rejected: Rejected,
    /// Whether the ledger holds an unresolved HIGH without a council YES (DES-TEAMING-001 §6.7):
    /// the run may not continue unattended. [`events::fold`] decides it from the stream.
    #[serde(default)]
    pub team_pause: bool,
}

impl TeamLedger {
    /// The one constructor: `teamPause` is computed from the ledger's own contents
    /// ([`events::pauses`]), never passed in, so no path can build an unpaused ledger that
    /// holds an unresolved HIGH. `renderedToJudge` starts false (S6 sets it when it renders).
    pub fn new(
        final_pass: FinalPass,
        monitors: Vec<LedgerMonitor>,
        findings: Vec<LedgerFinding>,
        rejected: Rejected,
    ) -> Self {
        let team_pause = events::pauses(final_pass, &findings);
        TeamLedger {
            final_pass,
            rendered_to_judge: false,
            monitors,
            findings,
            rejected,
            team_pause,
        }
    }

    /// Recompute `teamPause` after the findings or `finalPass` changed.
    pub fn refresh_pause(&mut self) {
        self.team_pause = events::ledger_pauses(self);
    }
}

/// [`TeamLedger`] as it arrives: every field that is a recorded fact, and no `teamPause` (an
/// incoming one is ignored as an unknown key).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TeamLedgerWire {
    final_pass: FinalPass,
    rendered_to_judge: bool,
    monitors: Vec<LedgerMonitor>,
    findings: Vec<LedgerFinding>,
    rejected: Rejected,
}

impl From<TeamLedgerWire> for TeamLedger {
    fn from(w: TeamLedgerWire) -> Self {
        let team_pause = events::pauses(w.final_pass, &w.findings);
        TeamLedger {
            final_pass: w.final_pass,
            rendered_to_judge: w.rendered_to_judge,
            monitors: w.monitors,
            findings: w.findings,
            rejected: w.rejected,
            team_pause,
        }
    }
}

/// What [`FindingBook::admit`] did with a confirmed, above-bar finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// First seen: emit it.
    New(Box<LedgerFinding>),
    /// Another monitor already raised it: recorded in `corroboratedBy`, not re-emitted.
    Corroborated,
    /// The same monitor repeated it: counted as `duplicate`, not re-emitted.
    Duplicate,
}

/// The attempt's findings, deduplicated by id (DES §4.6 step 4).
#[derive(Debug, Default)]
pub struct FindingBook {
    pub findings: Vec<LedgerFinding>,
    index: HashMap<String, usize>,
    pub rejected: Rejected,
}

impl FindingBook {
    /// Admit one confirmed, above-bar finding. Its `finding_id` is minted here from `path` and
    /// `evidence` (whatever the caller put there is replaced), so dedup and the id agree by
    /// construction.
    pub fn admit(&mut self, mut finding: Finding) -> Admit {
        let id = finding_id(&finding.path, &finding.evidence);
        if let Some(&i) = self.index.get(&id) {
            let f = &mut self.findings[i];
            // The same monitor raising it again — as its author or as a corroborator — is a
            // duplicate; another monitor raising it is a corroboration.
            if f.finding.monitor_id == finding.monitor_id
                || f.corroborated_by.contains(&finding.seat)
            {
                self.rejected.duplicate += 1;
                return Admit::Duplicate;
            }
            f.corroborated_by.push(finding.seat);
            return Admit::Corroborated;
        }
        finding.finding_id = id.clone();
        let f = LedgerFinding {
            finding,
            final_line: None,
            corroborated_by: Vec::new(),
            delivery: LedgerDelivery::NotDelivered,
            status: FindingStatus::Unanswered,
            worker_reason: None,
            monitor_reply: None,
            dispute: None,
        };
        self.index.insert(id, self.findings.len());
        self.findings.push(f.clone());
        Admit::New(Box::new(f))
    }
}

// ── The settled tree (DES §4.4) ───────────────────────────────────────────────────────────────────

/// The unit's worktree, read through its PINNED git dir (the baseline's), never the worker's
/// index: snapshots go through a scratch index (`worktree_guard::snapshot_through`); diffs and
/// file reads use the object database only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repo {
    pub workdir: PathBuf,
    pub git_dir: PathBuf,
}

impl Repo {
    fn env(&self) -> [(&str, &Path); 1] {
        [("GIT_DIR", self.git_dir.as_path())]
    }

    /// The worktree's content tree now.
    pub fn snapshot(&self) -> anyhow::Result<String> {
        Ok(crate::worktree_guard::snapshot_through(&self.workdir, &self.git_dir)?.tree)
    }

    /// `path` at `tree`, or `None` when it is absent (or not text).
    pub fn file(&self, tree: &str, path: &str) -> Option<String> {
        let spec = format!("{tree}:{path}");
        crate::worktree_guard::git(&self.workdir, &["cat-file", "-p", &spec], &self.env())
            .ok()
            .and_then(|b| String::from_utf8(b).ok())
    }

    /// The paths that differ between `from` and `to`.
    pub fn changed_paths(&self, from: &str, to: &str) -> BTreeSet<String> {
        crate::worktree_guard::git_string(
            &self.workdir,
            &["diff-tree", "-r", "--name-only", "--no-renames", from, to],
            &self.env(),
        )
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default()
    }

    /// The incremental patch `from`..`to`, capped at `cap` bytes. Past the cap the text carries
    /// the name-status list, then the patch up to the cap, and says so.
    pub fn diff(&self, from: &str, to: &str, cap: usize) -> String {
        let patch = crate::worktree_guard::git(
            &self.workdir,
            &["diff-tree", "-r", "-p", "-U3", "--no-renames", from, to],
            &self.env(),
        )
        .map(|b| String::from_utf8_lossy(&b).into_owned())
        .unwrap_or_else(|e| format!("[diff unavailable: {e}]"));
        if patch.len() <= cap {
            return patch;
        }
        let names = crate::worktree_guard::git_string(
            &self.workdir,
            &["diff-tree", "-r", "--name-status", "--no-renames", from, to],
            &self.env(),
        )
        .unwrap_or_default();
        let head = cap_utf8(&patch, cap.saturating_sub(names.len()));
        format!(
            "[diff truncated at {cap} bytes: every changed path is listed first; Read a file for \
             the rest]\n{names}\n{head}\n[… truncated]"
        )
    }
}

// ── The supervisor core (DES §4.1-4.3) ────────────────────────────────────────────────────────────

/// `(run, ord, attempt)`.
pub type UnitKey = (String, u32, u32);

/// What `TeamCmd::Attach` carries (DES §4.2): everything the supervisor needs for one attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachCtx {
    pub run_id: String,
    pub ord: u32,
    pub attempt: u32,
    /// The creator's seat instance (`unit.assigned_cli`) — never a monitor.
    pub creator: String,
    pub plan: TeamPlan,
    /// `None` when the unit is unbound or its dispatch baseline was not taken: not monitored,
    /// and said so.
    pub repo: Option<Repo>,
    /// The unit's dispatch baseline tree.
    pub baseline_tree: Option<String>,
    pub criterion: String,
    pub phase: String,
    pub code_graph_db: Option<String>,
}

impl AttachCtx {
    pub fn key(&self) -> UnitKey {
        (self.run_id.clone(), self.ord, self.attempt)
    }
}

/// Where a monitor session runs: a private scratch cwd (its ONLY write root), the run's graph,
/// and the unit's worktree as a READ root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorScope {
    pub cwd: PathBuf,
    pub code_graph_db: Option<String>,
    pub read_roots: Vec<String>,
}

/// The carrier a monitor runs on. Production: `AcpStepRunner` (`monitor_ensure`/`monitor_turn`
/// beside `chat_turn`, the chat boundary, no chat events). Tests: a fake.
pub trait MonitorHost: Send + Sync {
    /// DES §4.1 (b): the seat's `[cli.acp]` is admitted to input governance — the read-only
    /// boundary is enforced by answering `session/request_permission`, which an unadmitted
    /// adapter never sends.
    fn admitted(&self, seat: &str) -> Result<(), String>;
    /// Start (or reuse) the monitor session `pool_key` on `seat`, read-only over `scope`.
    fn open(&self, pool_key: &str, seat: &str, scope: &MonitorScope) -> Result<(), String>;
    /// One monitor turn; the reply text. `Err` means the turn failed AND the host closed the
    /// session (`AcpStepRunner::monitor_turn` evicts on any failure): the supervisor reopens it
    /// on the next batch through [`MonitorHost::open`].
    fn turn(&self, pool_key: &str, prompt: &str, budget: Duration) -> Result<String, String>;
    /// Close the session.
    fn close(&self, pool_key: &str);
}

/// The event sink (production: `Command::EmitEvent` on the actor channel).
pub type Emit = Arc<dyn Fn(CoreEvent) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotState {
    /// Admitted, session not started yet (monitors open lazily, with their first batch).
    Pending,
    Open,
    Failed,
}

#[derive(Debug, Clone)]
struct MonitorSlot {
    id: String,
    seat: String,
    pool_key: String,
    state: SlotState,
    batches: u32,
    budget_exhausted: bool,
    timed_out: bool,
    error: Option<String>,
    in_flight: bool,
    primed: bool,
    pending: bool,
    last_tree: String,
    last_start: Option<Instant>,
}

/// One attempt's team state.
#[derive(Debug)]
pub struct UnitTeam {
    ctx: AttachCtx,
    monitors: Vec<MonitorSlot>,
    summoned: bool,
    /// A tree-changing checkpoint arrived before any monitor was summoned.
    pending: bool,
    last_seq: u64,
    titles: Vec<String>,
    book: FindingBook,
}

impl UnitTeam {
    fn new(ctx: AttachCtx) -> Self {
        Self {
            ctx,
            monitors: Vec::new(),
            summoned: false,
            pending: false,
            last_seq: 0,
            titles: Vec::new(),
            book: FindingBook::default(),
        }
    }

    fn pool_key(&self, monitor_id: &str) -> String {
        format!(
            "team:{}:{}:{}:{monitor_id}",
            self.ctx.run_id, self.ctx.ord, self.ctx.attempt
        )
    }

    fn emit_attached(&self, emit: &Emit, id: &str, seat: &str, error: Option<String>) {
        emit(CoreEvent::MonitorAttached {
            session: self.ctx.run_id.clone(),
            ord: self.ctx.ord,
            attempt: self.ctx.attempt,
            monitor_id: id.to_string(),
            seat: seat.to_string(),
            status: if error.is_some() {
                "failed"
            } else {
                "attached"
            }
            .to_string(),
            reason: format!("team plan monitors={}", self.ctx.plan.monitors),
            error,
        });
    }

    /// Summon the plan's monitors (DES §4.1): the first `plan.monitors` candidates that are NOT
    /// the creator's instance and ARE admitted. A refused candidate is disclosed as
    /// `monitorAttached{failed}` and starts no process. `plan.monitors == 0` summons nothing and
    /// emits nothing.
    fn summon(&mut self, host: &dyn MonitorHost, emit: &Emit) {
        if self.summoned {
            return;
        }
        self.summoned = true;
        let want = usize::from(self.ctx.plan.monitors);
        if want == 0 {
            return;
        }
        let Some(baseline) = self
            .ctx
            .baseline_tree
            .clone()
            .filter(|_| self.ctx.repo.is_some())
        else {
            for (i, seat) in self.ctx.plan.candidates.iter().take(want).enumerate() {
                self.emit_attached(
                    emit,
                    &format!("m{}", i + 1),
                    seat,
                    Some("no worktree baseline".to_string()),
                );
            }
            return;
        };
        let mut admitted = 0;
        let candidates = self.ctx.plan.candidates.clone();
        for (i, seat) in candidates.iter().enumerate() {
            if admitted == want {
                break;
            }
            let id = format!("m{}", i + 1);
            let refusal = if seat == &self.ctx.creator {
                Some(format!(
                    "'{seat}' is the creator's own seat instance — a monitor must be distinct"
                ))
            } else {
                host.admitted(seat).err()
            };
            if let Some(why) = refusal {
                self.emit_attached(emit, &id, seat, Some(why));
                continue;
            }
            admitted += 1;
            let pool_key = self.pool_key(&id);
            self.monitors.push(MonitorSlot {
                id,
                seat: seat.clone(),
                pool_key,
                state: SlotState::Pending,
                batches: 0,
                budget_exhausted: false,
                timed_out: false,
                error: None,
                in_flight: false,
                primed: false,
                pending: self.pending,
                last_tree: baseline.clone(),
                last_start: None,
            });
        }
    }

    fn scope(&self, monitor_id: &str) -> MonitorScope {
        let workdir = self
            .ctx
            .repo
            .as_ref()
            .map(|r| r.workdir.to_string_lossy().into_owned());
        MonitorScope {
            cwd: crate::acp_runner::ChatScope::scratch_for(&self.pool_key(monitor_id)),
            code_graph_db: self.ctx.code_graph_db.clone(),
            read_roots: workdir.into_iter().collect(),
        }
    }

    fn header(&self) -> String {
        let workdir = self
            .ctx
            .repo
            .as_ref()
            .map(|r| r.workdir.display().to_string())
            .unwrap_or_default();
        format!(
            "[wicked-core · team monitor · READ-ONLY reviewer]\n\
             You are a peer monitor on a unit another agent is working on right now. You advise; \
             you do not decide. The worker may refuse you with evidence, and the gate decides.\n\
             Unit criterion: {criterion}\nPhase: {phase}\n\
             The worktree is {workdir}. You may Read files there for context; any write is \
             refused.\n\
             Report only defects you can pin to ONE line of the change: `high` (a real bug, data \
             loss, a security hole, or a caller the change breaks) or `medium` (a real defect \
             with limited reach). No style, naming or speculation. A finding is dropped unless \
             `evidence` is the exact text of line `line` of `path` in the tree you were shown.\n\
             Reply with zero or more lines of exactly\n\
             FINDING {{\"severity\":\"high|medium|low\",\"path\":\"<repo-relative path>\",\
             \"line\":<n>,\"evidence\":\"<the exact text of that line>\",\"claim\":\"<what is \
             wrong and why>\",\"suggestion\":\"<optional fix>\"}}\n\
             then a final line DONE. Anything else is ignored.\n",
            criterion = self.ctx.criterion,
            phase = self.ctx.phase,
        )
    }

    fn job(&self, i: usize, final_tree: Option<String>, budget: Duration, cap: usize) -> BatchJob {
        let m = &self.monitors[i];
        BatchJob {
            key: self.ctx.key(),
            slot: i,
            monitor_id: m.id.clone(),
            seat: m.seat.clone(),
            pool_key: m.pool_key.clone(),
            needs_open: m.state == SlotState::Pending,
            scope: self.scope(&m.id),
            header: (!m.primed).then(|| self.header()),
            titles: self.titles.clone(),
            from_tree: m.last_tree.clone(),
            baseline_tree: self.ctx.baseline_tree.clone().unwrap_or_default(),
            final_tree,
            checkpoint_seq: self.last_seq,
            repo: self.ctx.repo.clone(),
            budget,
            diff_cap: cap,
            run_id: self.ctx.run_id.clone(),
            ord: self.ctx.ord,
            attempt: self.ctx.attempt,
            reason: if m.batches > 0 {
                format!(
                    "team plan monitors={}; reopened after a failed turn",
                    self.ctx.plan.monitors
                )
            } else {
                format!("team plan monitors={}", self.ctx.plan.monitors)
            },
        }
    }

    /// Fold one batch's outcome in: dedup and emit the survivors.
    fn apply(&mut self, done: BatchDone, emit: &Emit) {
        let Some(m) = self.monitors.get_mut(done.slot) else {
            return;
        };
        m.in_flight = false;
        match done.outcome {
            BatchOutcome::OpenFailed(e) => {
                m.state = SlotState::Failed;
                m.error = Some(e);
            }
            BatchOutcome::SnapshotFailed(e) => m.error = Some(e),
            // The tree did not move: nothing opened, nothing spent.
            BatchOutcome::Skipped => {}
            // The host EVICTS the session on any failed turn (`monitor_turn` → `monitor_close`),
            // so the slot goes back to `Pending`: the next batch reopens it (`needs_open`) and
            // runs, instead of every later turn failing "monitor is not open" (codex, #609 r2).
            BatchOutcome::TurnFailed { error, timed_out } => {
                m.state = SlotState::Pending;
                m.batches += 1;
                m.primed = false;
                m.timed_out |= timed_out;
                m.error = Some(error);
            }
            BatchOutcome::Reviewed {
                tree,
                candidates,
                rejected,
            } => {
                m.state = SlotState::Open;
                m.batches += 1;
                m.primed = true;
                m.last_tree = tree.clone();
                let (id, seat) = (m.id.clone(), m.seat.clone());
                self.book.rejected.add(rejected);
                let key = self.ctx.key();
                for (raw, severity, in_diff) in candidates {
                    let finding = Finding {
                        finding_id: String::new(),
                        monitor_id: id.clone(),
                        seat: seat.clone(),
                        severity,
                        path: raw.path,
                        line: raw.line,
                        // Confirmed exact and within the cap (`run_job`): never truncated.
                        evidence: raw.evidence,
                        claim: cap_utf8(&raw.claim, CLAIM_CAP),
                        suggestion: raw.suggestion.as_deref().map(|s| cap_utf8(s, CLAIM_CAP)),
                        tree: tree.clone(),
                        in_diff,
                        checkpoint_seq: done.checkpoint_seq,
                    };
                    if let Admit::New(f) = self.book.admit(finding) {
                        emit(f.finding.event(&key));
                    }
                }
            }
        }
    }

    /// The ledger as it stands.
    pub fn ledger(&self, final_pass: FinalPass) -> TeamLedger {
        let monitors = self
            .monitors
            .iter()
            .map(|m| LedgerMonitor {
                monitor_id: m.id.clone(),
                seat: m.seat.clone(),
                batches: m.batches,
                status: if m.state == SlotState::Failed {
                    MonitorStatus::Failed
                } else if m.budget_exhausted {
                    MonitorStatus::BudgetExhausted
                } else if m.timed_out {
                    MonitorStatus::TimedOut
                } else {
                    MonitorStatus::Completed
                },
                error: m.error.clone(),
            })
            .collect();
        TeamLedger::new(
            final_pass,
            monitors,
            self.book.findings.clone(),
            self.book.rejected,
        )
    }
}

/// One batch, runnable off the supervisor (it owns everything it reads).
#[derive(Debug, Clone)]
pub struct BatchJob {
    pub key: UnitKey,
    slot: usize,
    monitor_id: String,
    seat: String,
    pool_key: String,
    needs_open: bool,
    scope: MonitorScope,
    header: Option<String>,
    titles: Vec<String>,
    from_tree: String,
    baseline_tree: String,
    /// `Some` at the final pass: the tree is already snapshotted.
    final_tree: Option<String>,
    checkpoint_seq: u64,
    repo: Option<Repo>,
    budget: Duration,
    diff_cap: usize,
    run_id: String,
    ord: u32,
    attempt: u32,
    reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum BatchOutcome {
    OpenFailed(String),
    SnapshotFailed(String),
    /// The tree id equals the last batch's: no model turn.
    Skipped,
    TurnFailed {
        error: String,
        timed_out: bool,
    },
    Reviewed {
        tree: String,
        candidates: Vec<(RawFinding, Severity, bool)>,
        rejected: Rejected,
    },
}

/// A finished batch, folded back in by the supervisor.
#[derive(Debug, Clone)]
pub struct BatchDone {
    pub key: UnitKey,
    slot: usize,
    checkpoint_seq: u64,
    outcome: BatchOutcome,
}

/// Run one batch (DES §4.3-4.6): snapshot once and skip when the tree is unchanged; else open the
/// session if this is its first reviewed batch (disclosing `monitorAttached` — monitors open
/// lazily), prompt the monitor with the incremental diff, and put every `FINDING` through
/// parse → bar → confirm.
pub fn run_job(job: &BatchJob, host: &dyn MonitorHost, emit: &Emit) -> BatchDone {
    let done = |outcome| BatchDone {
        key: job.key.clone(),
        slot: job.slot,
        checkpoint_seq: job.checkpoint_seq,
        outcome,
    };
    let Some(repo) = job.repo.as_ref() else {
        return done(BatchOutcome::SnapshotFailed("no worktree".to_string()));
    };
    let tree = match job.final_tree.clone() {
        Some(t) => t,
        None => match repo.snapshot() {
            Ok(t) => t,
            Err(e) => return done(BatchOutcome::SnapshotFailed(e.to_string())),
        },
    };
    // An `execute` that wrote nothing costs one snapshot — no session, no model turn.
    if tree == job.from_tree {
        return done(BatchOutcome::Skipped);
    }
    if job.needs_open {
        let opened = host.open(&job.pool_key, &job.seat, &job.scope);
        emit(CoreEvent::MonitorAttached {
            session: job.run_id.clone(),
            ord: job.ord,
            attempt: job.attempt,
            monitor_id: job.monitor_id.clone(),
            seat: job.seat.clone(),
            status: if opened.is_ok() { "attached" } else { "failed" }.to_string(),
            reason: job.reason.clone(),
            error: opened.as_ref().err().cloned(),
        });
        if let Err(e) = opened {
            return done(BatchOutcome::OpenFailed(e));
        }
    }
    let mut prompt = job.header.clone().unwrap_or_default();
    prompt.push_str(&format!(
        "\n[{} — tree {tree}; the change since tree {}]\n",
        if job.final_tree.is_some() {
            "final pass: the worker's turn has ended, this is the settled tree"
        } else {
            "batch"
        },
        job.from_tree
    ));
    if !job.titles.is_empty() {
        prompt.push_str("Tool calls since the last batch:\n");
        for t in job.titles.iter().rev().take(TITLES_PER_BATCH).rev() {
            prompt.push_str(&format!("- {}\n", cap_utf8(t, TITLE_CAP)));
        }
    }
    prompt.push_str("```diff\n");
    prompt.push_str(&repo.diff(&job.from_tree, &tree, job.diff_cap));
    prompt.push_str("\n```\n");
    let started = Instant::now();
    let reply = match host.turn(&job.pool_key, &prompt, job.budget) {
        Ok(r) => r,
        Err(error) => {
            let timed_out = started.elapsed() >= job.budget;
            return done(BatchOutcome::TurnFailed { error, timed_out });
        }
    };
    let (parsed, malformed) = parse_reply(&reply);
    let mut rejected = Rejected {
        malformed,
        ..Rejected::default()
    };
    let in_diff_paths = repo.changed_paths(&job.baseline_tree, &tree);
    let mut candidates = Vec::new();
    for f in parsed {
        let Some(severity) = Severity::parse(&f.severity) else {
            rejected.below_bar += 1;
            continue;
        };
        // `evidence` IS the line text (DES §7: ≤ 512 B). A longer quote is refused HERE, as
        // unconfirmed — never confirmed exact and then truncated, which would emit evidence that
        // is not the line and let the final re-confirmation call a present line superseded
        // (codex, #609 r2). Said in the daemon log, since `rejected` carries counts only.
        if f.evidence.len() > EVIDENCE_CAP {
            rejected.unconfirmed += 1;
            eprintln!(
                "[wicked-core] team: {}:{}:{} monitor {} finding at {}:{} rejected as unconfirmed: \
                 evidence over cap ({} B > {EVIDENCE_CAP} B)",
                job.run_id,
                job.ord,
                job.attempt,
                job.monitor_id,
                f.path,
                f.line,
                f.evidence.len()
            );
            continue;
        }
        if !confirm(repo.file(&tree, &f.path).as_deref(), f.line, &f.evidence) {
            rejected.unconfirmed += 1;
            continue;
        }
        let in_diff = in_diff_paths.contains(&f.path);
        candidates.push((f, severity, in_diff));
    }
    done(BatchOutcome::Reviewed {
        tree,
        candidates,
        rejected,
    })
}

/// Attempts whose checkpoints may be held before their `Attach` lands, and checkpoints per such
/// attempt (see [`TeamCore::hold_early`]). Past either bound the OLDEST is dropped, and the daemon
/// log says so.
pub const EARLY_CHECKPOINT_KEYS: usize = 32;
pub const EARLY_CHECKPOINTS_PER_KEY: usize = 64;

/// The supervisor's state: every attached attempt.
pub struct TeamCore {
    units: HashMap<UnitKey, Arc<Mutex<UnitTeam>>>,
    /// Checkpoints that arrived before their attempt's `Attach` (oldest attempt first).
    early: Vec<(UnitKey, Vec<CoreEvent>)>,
    host: Arc<dyn MonitorHost>,
    emit: Emit,
    pub limits: TeamLimits,
}

impl TeamCore {
    pub fn new(host: Arc<dyn MonitorHost>, emit: Emit, limits: TeamLimits) -> Self {
        Self {
            units: HashMap::new(),
            early: Vec::new(),
            host,
            emit,
            limits,
        }
    }

    /// `TeamCmd::Attach`: start tracking an attempt (idempotent per key), then replay, in order,
    /// any checkpoint of it that arrived first.
    pub fn attach(&mut self, ctx: AttachCtx) {
        let key = ctx.key();
        self.units
            .entry(key.clone())
            .or_insert_with(|| Arc::new(Mutex::new(UnitTeam::new(ctx))));
        if let Some(pos) = self.early.iter().position(|(k, _)| *k == key) {
            let (_, held) = self.early.remove(pos);
            for ev in held {
                self.on_event(&ev);
            }
        }
    }

    /// A checkpoint for an attempt this core is not tracking (yet). `Attach` travels the direct
    /// command channel from the worker thread while checkpoints come through the engine's
    /// fan-out and a forwarder thread (DES §4.2), so a fast first tool call can overtake its own
    /// attach; dropping it would silence the monitor for a unit whose only tree change it was.
    /// It is held until the attach lands, bounded on both axes.
    fn hold_early(&mut self, key: UnitKey, ev: CoreEvent) {
        let pos = match self.early.iter().position(|(k, _)| *k == key) {
            Some(p) => p,
            None => {
                if self.early.len() >= EARLY_CHECKPOINT_KEYS {
                    let (k, held) = self.early.remove(0);
                    eprintln!(
                        "[wicked-core] team: {} checkpoint(s) of {}:{}:{} arrived before its \
                         attach and were dropped ({EARLY_CHECKPOINT_KEYS} attempts already held)",
                        held.len(),
                        k.0,
                        k.1,
                        k.2
                    );
                }
                self.early.push((key, Vec::new()));
                self.early.len() - 1
            }
        };
        let (k, held) = &mut self.early[pos];
        if held.len() >= EARLY_CHECKPOINTS_PER_KEY {
            held.remove(0);
            eprintln!(
                "[wicked-core] team: the oldest checkpoint of {}:{}:{} was dropped \
                 ({EARLY_CHECKPOINTS_PER_KEY} already held before its attach)",
                k.0, k.1, k.2
            );
        }
        held.push(ev);
    }

    /// Consume one engine event. Only `unitCheckpoint` is read (DES §4.2): deltas, hook replays
    /// and post-unit tool events never reach a monitor.
    pub fn on_event(&mut self, ev: &CoreEvent) {
        let CoreEvent::UnitCheckpoint {
            session,
            ord,
            attempt,
            seq,
            kind,
            title,
            ..
        } = ev
        else {
            return;
        };
        let key = (session.clone(), *ord, *attempt);
        let Some(unit) = self.units.get(&key) else {
            self.hold_early(key, ev.clone());
            return;
        };
        let mut u = unit.lock().unwrap_or_else(|p| p.into_inner());
        u.last_seq = u.last_seq.max(*seq);
        u.titles.push(title.clone());
        if kind_may_change_tree(kind) {
            u.pending = true;
            for m in &mut u.monitors {
                m.pending = true;
            }
        }
    }

    /// The batches due at `now` (DES §4.3): a monitor with a pending tree change, no batch in
    /// flight, budget left, and at least `batch_min_interval` since its previous batch started.
    /// Monitors are summoned here — lazily, when the first batch is due.
    pub fn due_jobs(&mut self, now: Instant) -> Vec<BatchJob> {
        let mut jobs = Vec::new();
        for unit in self.units.values() {
            let mut u = unit.lock().unwrap_or_else(|p| p.into_inner());
            if !u.summoned && u.pending {
                u.summon(&*self.host, &self.emit);
            }
            let mut took_titles = false;
            for i in 0..u.monitors.len() {
                let limits = self.limits;
                let m = &mut u.monitors[i];
                if !m.pending || m.in_flight || m.state == SlotState::Failed {
                    continue;
                }
                if m.batches >= limits.max_batches {
                    m.budget_exhausted = true;
                    m.pending = false;
                    continue;
                }
                if m.last_start
                    .is_some_and(|t| now.duration_since(t) < limits.batch_min_interval)
                {
                    continue;
                }
                m.pending = false;
                m.in_flight = true;
                m.last_start = Some(now);
                jobs.push(u.job(i, None, limits.monitor_turn_budget, limits.diff_cap));
                took_titles = true;
            }
            if took_titles {
                u.titles.clear();
            }
        }
        jobs
    }

    /// Fold a finished batch back in. A batch for an attempt already finished (abandoned by the
    /// final pass) is dropped.
    pub fn apply(&mut self, done: BatchDone) {
        if let Some(unit) = self.units.get(&done.key) {
            unit.lock()
                .unwrap_or_else(|p| p.into_inner())
                .apply(done, &self.emit);
        }
    }

    /// `TeamCmd::Finish`: hand the attempt to its final pass and stop tracking it. An attempt
    /// that was never attached (a wrapped carrier) starts from `ctx`.
    pub fn take(&mut self, ctx: AttachCtx) -> Arc<Mutex<UnitTeam>> {
        let key = ctx.key();
        self.early.retain(|(k, _)| *k != key);
        self.units
            .remove(&key)
            .unwrap_or_else(|| Arc::new(Mutex::new(UnitTeam::new(ctx))))
    }

    /// Stop tracking every attempt of `run_id`.
    pub fn drop_run(&mut self, run_id: &str) {
        self.units.retain(|(r, _, _), _| r != run_id);
        self.early.retain(|(k, _)| k.0 != run_id);
    }

    pub fn host(&self) -> Arc<dyn MonitorHost> {
        Arc::clone(&self.host)
    }

    pub fn emit(&self) -> Emit {
        Arc::clone(&self.emit)
    }
}

/// The final pass (DES §4.7 steps 1-3). For a unit that did not end `Ok`, the pass is skipped
/// (`finalPass: "skipped"`). Otherwise: snapshot `T_final`; summon the plan's monitors if none are
/// attached yet (how a wrapped unit is monitored); run one final batch per live monitor over its
/// last tree..`T_final` while the budget lasts; re-confirm every finding against `T_final`
/// (`finalLine`, or `superseded` when its text is gone). Every monitor session is closed.
///
/// S2's final batch carries NO worker declines: S3 parses the worker's `ADVICE` lines and adds
/// the HOLD/WITHDRAW question (DES §4.7 step 2 needs step 4's output).
pub fn final_pass(
    unit: &Arc<Mutex<UnitTeam>>,
    host: &dyn MonitorHost,
    emit: &Emit,
    limits: TeamLimits,
    ok: bool,
    deadline: Instant,
) -> TeamLedger {
    let close_all = |u: &UnitTeam| {
        for m in &u.monitors {
            if m.state != SlotState::Pending {
                host.close(&m.pool_key);
            }
        }
    };
    if !ok {
        let u = unit.lock().unwrap_or_else(|p| p.into_inner());
        close_all(&u);
        return u.ledger(FinalPass::Skipped);
    }
    let (repo, jobs) = {
        let mut u = unit.lock().unwrap_or_else(|p| p.into_inner());
        let Some(repo) = u.ctx.repo.clone() else {
            u.summon(host, emit);
            return u.ledger(FinalPass::Skipped);
        };
        let t_final = match repo.snapshot() {
            Ok(t) => t,
            Err(e) => {
                for m in &mut u.monitors {
                    m.error = Some(format!("final snapshot failed: {e}"));
                }
                close_all(&u);
                return u.ledger(FinalPass::Skipped);
            }
        };
        u.summon(host, emit);
        let mut jobs = Vec::new();
        for i in 0..u.monitors.len() {
            let m = &mut u.monitors[i];
            if m.state == SlotState::Failed {
                continue;
            }
            if m.batches >= limits.max_batches {
                m.budget_exhausted = true;
                continue;
            }
            m.in_flight = true;
            jobs.push((
                i,
                u.job(
                    i,
                    Some(t_final.clone()),
                    limits.monitor_turn_budget,
                    limits.diff_cap,
                ),
            ));
        }
        (repo, (t_final, jobs))
    };
    let (t_final, jobs) = jobs;
    let mut timed_out = false;
    for (i, mut job) in jobs {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            timed_out = true;
            let mut u = unit.lock().unwrap_or_else(|p| p.into_inner());
            u.monitors[i].in_flight = false;
            u.monitors[i].timed_out = true;
            continue;
        }
        job.budget = job.budget.min(left);
        let done = run_job(&job, host, emit);
        unit.lock()
            .unwrap_or_else(|p| p.into_inner())
            .apply(done, emit);
    }
    let mut u = unit.lock().unwrap_or_else(|p| p.into_inner());
    for f in &mut u.book.findings {
        let text = repo.file(&t_final, &f.finding.path);
        match locate(text.as_deref(), f.finding.line, &f.finding.evidence) {
            Some(n) => f.final_line = Some(n),
            None => {
                f.final_line = None;
                f.status = FindingStatus::Superseded;
            }
        }
    }
    close_all(&u);
    u.ledger(if timed_out {
        FinalPass::TimedOut
    } else {
        FinalPass::Completed
    })
}

// ── The supervisor thread (DES §3, §4.2) ──────────────────────────────────────────────────────────

/// The supervisor's commands (DES §4.2 names `Attach` and `Finish`).
pub enum TeamCmd {
    Attach(AttachCtx),
    /// Hand the attempt over for its final pass; the reply is the attempt's state.
    Finish(AttachCtx, Sender<Arc<Mutex<UnitTeam>>>),
    /// The run ended: forget its attempts.
    RunComplete(String),
}

enum TeamMsg {
    Cmd(TeamCmd),
    Event(Box<CoreEvent>),
    Done(BatchDone),
}

/// A handle on the running supervisor.
#[derive(Clone)]
pub struct TeamHandle {
    tx: Sender<TeamMsg>,
    pub limits: TeamLimits,
    host: Arc<dyn MonitorHost>,
    emit: Emit,
}

impl TeamHandle {
    pub fn send(&self, cmd: TeamCmd) {
        let _ = self.tx.send(TeamMsg::Cmd(cmd));
    }

    /// `StepRunner::team_finish`'s engine: take the attempt, run its final pass on a thread, and
    /// wait at most `final_pass_budget` — on expiry the ledger says `timed_out` with what was
    /// gathered, and the gate proceeds.
    pub fn finish(&self, ctx: AttachCtx, ok: bool) -> Option<TeamLedger> {
        let (reply_tx, reply_rx) = std::sync::mpsc::channel();
        self.send(TeamCmd::Finish(ctx, reply_tx));
        let unit = reply_rx.recv_timeout(Duration::from_secs(30)).ok()?;
        let deadline = Instant::now() + self.limits.final_pass_budget;
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let (u, host, emit, limits) = (
            Arc::clone(&unit),
            Arc::clone(&self.host),
            Arc::clone(&self.emit),
            self.limits,
        );
        std::thread::spawn(move || {
            let _ = done_tx.send(final_pass(&u, &*host, &emit, limits, ok, deadline));
        });
        match done_rx.recv_timeout(self.limits.final_pass_budget) {
            Ok(ledger) => Some(ledger),
            Err(_) => Some(
                unit.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .ledger(FinalPass::TimedOut),
            ),
        }
    }
}

/// Start the per-daemon supervisor. `events` is the engine's fan-out (`Command::Subscribe`), read
/// on a forwarder thread; `host` runs monitor sessions; `emit` reaches the actor's single emit
/// point. The supervisor ticks once a second so a batch whose interval elapses with no new
/// checkpoint still starts.
pub fn spawn_supervisor(
    host: Arc<dyn MonitorHost>,
    emit: Emit,
    events: Receiver<CoreEvent>,
    limits: TeamLimits,
) -> TeamHandle {
    let (tx, rx) = std::sync::mpsc::channel::<TeamMsg>();
    let fwd = tx.clone();
    std::thread::spawn(move || {
        for ev in events {
            if matches!(ev, CoreEvent::UnitCheckpoint { .. })
                && fwd.send(TeamMsg::Event(Box::new(ev))).is_err()
            {
                break;
            }
        }
    });
    let handle = TeamHandle {
        tx: tx.clone(),
        limits,
        host: Arc::clone(&host),
        emit: Arc::clone(&emit),
    };
    let done_tx = tx;
    std::thread::spawn(move || {
        let mut core = TeamCore::new(host, emit, limits);
        loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(TeamMsg::Cmd(TeamCmd::Attach(ctx))) => core.attach(ctx),
                Ok(TeamMsg::Cmd(TeamCmd::Finish(ctx, reply))) => {
                    let _ = reply.send(core.take(ctx));
                }
                Ok(TeamMsg::Cmd(TeamCmd::RunComplete(run))) => core.drop_run(&run),
                Ok(TeamMsg::Event(ev)) => core.on_event(&ev),
                Ok(TeamMsg::Done(done)) => core.apply(done),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            for job in core.due_jobs(Instant::now()) {
                let (host, emit, back) = (core.host(), core.emit(), done_tx.clone());
                std::thread::spawn(move || {
                    let _ = back.send(TeamMsg::Done(run_job(&job, &*host, &emit)));
                });
            }
        }
    });
    handle
}

// ── DES-TEAMING-001 S3 (#602): monitor→worker advice over `_session/steering` ─────────────────

/// The JSON-RPC method the adapter registers for a mid-turn steer (`acp-agent.js:103`, `:7503`).
pub(crate) const STEER_METHOD: &str = "_session/steering";

/// The one advice text cap (DES §5.3): the same 8 KB as an elicitation message.
pub(crate) const ADVICE_TEXT_CAP: usize = 8 * 1024;

/// A worker's `ADVICE` reason is capped at 2 KB on the wire (DES §7).
pub(crate) const REASON_CAP: usize = 2 * 1024;

/// A steering JSON-RPC error's message is carried as `detail`, capped here.
pub(crate) const DETAIL_CAP: usize = 512;

/// `carrier` on `adviceDelivered` for a real steering request (DES §7).
pub(crate) const CARRIER_ACP_STEERING: &str = "acp_steering";

/// `carrier` on `adviceDelivered{outcome:"not_delivered"}` when the unit's carrier has no
/// mid-turn channel at all (wrapped, PTY, an ACP adapter that did not advertise steering).
pub(crate) const CARRIER_NONE: &str = "none";

/// A finding's severity. Only `high` is ever steered into the worker (DES §4.6 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    High,
    Medium,
}

impl serde::Serialize for Severity {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for Severity {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Severity::parse(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("severity `{s}` is below the bar")))
    }
}

impl Severity {
    /// The bar (DES §4.6 step 2): `high` | `medium`; anything else is below it.
    pub(crate) fn parse(s: &str) -> Option<Self> {
        match s {
            "high" => Some(Severity::High),
            "medium" => Some(Severity::Medium),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
        }
    }
}

/// A confirmed monitor finding, as `monitorFinding` shapes it (DES §7): THE one finding type.
/// S2's confirmation constructs it, the ledger embeds it ([`LedgerFinding`]), and S3 steers it.
///
/// **Deserialized through [`FindingWire`]:** `findingId` is computed from `path` and
/// `evidence` ([`finding_id`]), never read from input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", from = "FindingWire")]
pub struct Finding {
    /// `"f-" + hex(sha256(path ‖ "\n" ‖ normalized evidence))[..16]` (DES §4.6 step 4).
    pub finding_id: String,
    pub monitor_id: String,
    /// The monitor's seat instance, e.g. `claude#2`.
    pub seat: String,
    pub severity: Severity,
    /// Repo-relative.
    pub path: String,
    pub line: u32,
    /// The exact text of that line in the snapshot tree (≤512 B).
    pub evidence: String,
    pub claim: String,
    pub suggestion: Option<String>,
    /// The snapshot tree id the finding was confirmed against.
    pub tree: String,
    pub in_diff: bool,
    /// The last checkpoint the batch that raised it covered. It rides `monitorFinding` only: the
    /// ledger's finding (DES §7 `teamLedger.findings[]`) carries no `checkpointSeq`.
    #[serde(skip)]
    pub checkpoint_seq: u64,
}

/// [`Finding`] as it arrives: no `findingId` (an incoming one is ignored and recomputed).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FindingWire {
    monitor_id: String,
    seat: String,
    severity: Severity,
    path: String,
    line: u32,
    evidence: String,
    claim: String,
    suggestion: Option<String>,
    tree: String,
    in_diff: bool,
}

impl From<FindingWire> for Finding {
    fn from(w: FindingWire) -> Self {
        Finding {
            finding_id: finding_id(&w.path, &w.evidence),
            monitor_id: w.monitor_id,
            seat: w.seat,
            severity: w.severity,
            path: w.path,
            line: w.line,
            evidence: w.evidence,
            claim: w.claim,
            suggestion: w.suggestion,
            tree: w.tree,
            in_diff: w.in_diff,
            checkpoint_seq: 0,
        }
    }
}

impl Finding {
    /// The `monitorFinding` event for this finding on attempt `key` (DES §7).
    pub(crate) fn event(&self, key: &UnitKey) -> CoreEvent {
        CoreEvent::MonitorFinding {
            session: key.0.clone(),
            ord: key.1,
            attempt: key.2,
            finding_id: self.finding_id.clone(),
            monitor_id: self.monitor_id.clone(),
            seat: self.seat.clone(),
            severity: self.severity.as_str().to_string(),
            path: self.path.clone(),
            line: self.line,
            evidence: self.evidence.clone(),
            claim: self.claim.clone(),
            suggestion: self.suggestion.clone(),
            tree: self.tree.clone(),
            in_diff: self.in_diff,
            checkpoint_seq: self.checkpoint_seq,
        }
    }
}

/// One queued piece of advice. The DES's mailbox is `Vec<Advice>` (§5.2); an advice is a HIGH
/// finding waiting for the next tool-call boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Advice {
    pub finding: Finding,
}

/// Where one queued finding ended up, as far as delivery goes (the ledger's `delivery` and the
/// `superseded` status, DES §7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Delivery {
    /// The adapter answered the steer `{"outcome":"injected"}`.
    Injected,
    /// Not delivered mid-turn. `detail` says why (turn ended, refused, no channel, …).
    NotDelivered { detail: String },
    /// Its evidence text was gone from a fresh snapshot at the delivery point (DES §5.2).
    Superseded,
}

/// The worker's disposition on one delivered finding (DES §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Accepted,
    Declined,
}

impl Disposition {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Disposition::Accepted => "accepted",
            Disposition::Declined => "declined",
        }
    }
}

/// One parsed `ADVICE` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdviceResponse {
    pub finding_id: String,
    pub disposition: Disposition,
    /// May be `""`: a refusal with no evidence is recorded as exactly that (DES §5.3).
    pub reason: String,
}

/// `adviceDelivered.outcome` (DES §7), plus `not_delivered` for a finding that never rode a
/// steering request (see [`CoreEvent::AdviceDelivered`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SteerOutcome {
    Injected,
    TurnEnded,
    Refused,
    NotDelivered,
}

impl SteerOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            SteerOutcome::Injected => "injected",
            SteerOutcome::TurnEnded => "turn_ended",
            SteerOutcome::Refused => "refused",
            SteerOutcome::NotDelivered => "not_delivered",
        }
    }
}

/// Everything S3 knows about one attempt's advice. S2's final pass and S6's ledger read it
/// through [`SteerMailbox::take_record`]; the same facts are on the event log as
/// `adviceDelivered` / `workerAdviceResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct AttemptAdvice {
    /// Per finding id, in id order.
    pub deliveries: BTreeMap<String, Delivery>,
    /// Per delivered finding id the worker answered: the LAST `ADVICE` line for it.
    pub responses: BTreeMap<String, AdviceResponse>,
    /// Delivered (injected) ids the worker's final output did not answer.
    pub unanswered: Vec<String>,
    /// Whether a turn of this attempt ran on a carrier that CAN take a steer.
    pub steering_channel: bool,
}

#[derive(Default)]
struct MailboxInner {
    queued: HashMap<UnitKey, Vec<Advice>>,
    records: HashMap<UnitKey, AttemptAdvice>,
}

/// `AcpStepRunner.steer_mailbox` (DES §5.2): written by the supervisor after a HIGH
/// `monitorFinding`, drained by the ACP carrier at a terminal `tool_call_update`, and swept at
/// the end of the attempt's turn. One per engine; cloning shares it.
#[derive(Clone, Default)]
pub(crate) struct SteerMailbox {
    inner: Arc<Mutex<MailboxInner>>,
}

impl SteerMailbox {
    fn lock(&self) -> std::sync::MutexGuard<'_, MailboxInner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Queue a finding for the next tool-call boundary of `key`'s turn. Refused (returns
    /// `false`) for anything below HIGH — a MEDIUM finding reaches the gate only (DES §4.6) — and
    /// for an id this attempt already holds, queued or recorded (dedup is S2's; this only makes
    /// a double write harmless).
    // Written by S2's supervisor (#601); until it lands only the tests call it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn queue(&self, key: UnitKey, finding: Finding) -> bool {
        if finding.severity != Severity::High {
            return false;
        }
        let mut g = self.lock();
        let known = g
            .records
            .get(&key)
            .is_some_and(|r| r.deliveries.contains_key(&finding.finding_id));
        let queued = g.queued.entry(key).or_default();
        if known
            || queued
                .iter()
                .any(|a| a.finding.finding_id == finding.finding_id)
        {
            return false;
        }
        queued.push(Advice { finding });
        true
    }

    /// Take everything queued for `key` (the delivery point drains ALL of it, DES §5.2).
    pub(crate) fn take_queued(&self, key: &UnitKey) -> Vec<Advice> {
        self.lock().queued.remove(key).unwrap_or_default()
    }

    /// Put advice back at the FRONT of `key`'s queue (what did not fit the 8 KB block).
    pub(crate) fn requeue_front(&self, key: &UnitKey, advice: Vec<Advice>) {
        if advice.is_empty() {
            return;
        }
        let mut g = self.lock();
        let q = g.queued.entry(key.clone()).or_default();
        let rest = std::mem::take(q);
        q.extend(advice);
        q.extend(rest);
    }

    pub(crate) fn record(&self, key: &UnitKey, finding_id: &str, delivery: Delivery) {
        self.lock()
            .records
            .entry(key.clone())
            .or_default()
            .deliveries
            .insert(finding_id.to_string(), delivery);
    }

    pub(crate) fn mark_steering_channel(&self, key: &UnitKey) {
        self.lock()
            .records
            .entry(key.clone())
            .or_default()
            .steering_channel = true;
    }

    fn with_record<T>(&self, key: &UnitKey, f: impl FnOnce(&mut AttemptAdvice) -> T) -> T {
        f(self.lock().records.entry(key.clone()).or_default())
    }

    /// Read a copy of `key`'s record without consuming it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn record_of(&self, key: &UnitKey) -> Option<AttemptAdvice> {
        self.lock().records.get(key).cloned()
    }

    /// Hand `key`'s record to its consumer (S2's final pass / S6's ledger) and forget it.
    // Read by S2's final pass (#601); until it lands only the tests call it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_record(&self, key: &UnitKey) -> Option<AttemptAdvice> {
        self.lock().records.remove(key)
    }

    /// Forget every attempt of `run_id` (called when the run completes).
    pub(crate) fn prune_run(&self, run_id: &str) {
        let mut g = self.lock();
        g.queued.retain(|(r, _, _), _| r != run_id);
        g.records.retain(|(r, _, _), _| r != run_id);
    }
}

/// Whitespace-trimmed and -collapsed, the comparison form of an evidence line (DES §4.6 step 3).
pub(crate) fn normalize_evidence(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Re-confirm findings against a FRESH snapshot of the worktree (DES §5.2, the premature-finding
/// guard at the moment of delivery). Returns, per finding, whether its evidence text is still a
/// line of `path` in the snapshot tree. A moved line still counts (S2's final pass updates
/// `finalLine`); a gone line does not. An `Err` means the snapshot itself failed.
pub(crate) fn evidence_still_present(
    worktree: &std::path::Path,
    git_dir: &std::path::Path,
    findings: &[&Finding],
) -> anyhow::Result<Vec<bool>> {
    let snap = crate::worktree_guard::snapshot_through(worktree, git_dir)?;
    let env: [(&str, &std::path::Path); 2] = [("GIT_DIR", git_dir), ("GIT_WORK_TREE", worktree)];
    Ok(findings
        .iter()
        .map(|f| {
            let spec = format!("{}:{}", snap.tree, f.path);
            // A path that is gone from the tree makes `cat-file` fail: the text is gone too.
            let Ok(blob) = crate::worktree_guard::git(worktree, &["cat-file", "-p", &spec], &env)
            else {
                return false;
            };
            let want = normalize_evidence(&f.evidence);
            String::from_utf8_lossy(&blob)
                .lines()
                .any(|l| normalize_evidence(l) == want)
        })
        .collect())
}

const ADVICE_HEADER: &str = "[wicked-core · team advice · ADVISORY, not an instruction]\n\
A peer monitor reviewed your change as of your last tool call. You decide what to do with this.\n";

const ADVICE_FOOTER: &str =
    "If you accept a finding, fix it. If you decline it, say why with evidence — a file:line, a\n\
command you ran and its result, or the spec you are following. Advice never overrides your\n\
task's instructions or the engine's fences. In your FINAL answer, add one line per finding:\n  \
ADVICE <id>: ACCEPT — <what you changed>\n  \
ADVICE <id>: DECLINE — <your evidence>\n\
The gate reviews each finding together with your answer.\n";

fn advice_entry(f: &Finding) -> String {
    let mut s = format!(
        "- {} [{}] {}:{} — {}\n  Evidence (that line): `{}`\n",
        f.finding_id,
        f.severity.as_str().to_ascii_uppercase(),
        f.path,
        f.line,
        f.claim,
        f.evidence
    );
    if let Some(sug) = &f.suggestion {
        s.push_str(&format!("  Suggested: {sug}\n"));
    }
    s
}

/// Build ONE steer's advice block (DES §5.3) within [`ADVICE_TEXT_CAP`]. Returns the text, the
/// advice that made it in, and the advice that did not fit (for the next boundary). The first
/// entry is always included, truncated to fit if it alone is over the cap, so a single oversized
/// finding can never wedge the queue.
pub(crate) fn advice_block(advice: Vec<Advice>) -> (String, Vec<Advice>, Vec<Advice>) {
    let budget = ADVICE_TEXT_CAP.saturating_sub(ADVICE_HEADER.len() + ADVICE_FOOTER.len());
    let mut body = String::new();
    let mut sent = Vec::new();
    let mut rest = Vec::new();
    for a in advice {
        let entry = advice_entry(&a.finding);
        if !rest.is_empty() || (!sent.is_empty() && body.len() + entry.len() > budget) {
            rest.push(a);
            continue;
        }
        if sent.is_empty() && entry.len() > budget {
            body.push_str(&cap_utf8(&entry, budget.saturating_sub(1)));
            body.push('\n');
        } else {
            body.push_str(&entry);
        }
        sent.push(a);
    }
    (format!("{ADVICE_HEADER}{body}{ADVICE_FOOTER}"), sent, rest)
}

/// The `_session/steering` params (DES §5.2). `idleBehavior: "promptRequired"` is not optional:
/// without it a steer that lands after the turn settled starts a DETACHED turn
/// (`acp-agent.js:1233-1242`) the engine would attribute to nothing.
pub(crate) fn steer_params(session_id: &str, text: &str) -> serde_json::Value {
    serde_json::json!({
        "sessionId": session_id,
        "prompt": [{"type": "text", "text": text}],
        "_meta": {"steering": {"idleBehavior": "promptRequired"}}
    })
}

/// The adapter's answer to a steering request → the `adviceDelivered` outcome and detail
/// (DES §5.2): `{"outcome":"injected"}` → `injected`; `{"outcome":"promptRequired"}` →
/// `turn_ended`; a JSON-RPC error → `refused` with the error. Any other outcome (e.g.
/// `startedNewTurn`, which `promptRequired` exists to prevent) is `refused` and named.
pub(crate) fn classify_steer_answer(v: &serde_json::Value) -> (SteerOutcome, Option<String>) {
    if let Some(err) = v.get("error") {
        let msg = err
            .get("message")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        let code = err.get("code").and_then(serde_json::Value::as_i64);
        let detail = match code {
            Some(c) => format!("{c}: {msg}"),
            None => msg,
        };
        return (SteerOutcome::Refused, Some(cap_utf8(&detail, DETAIL_CAP)));
    }
    let result = &v["result"];
    match result["outcome"].as_str() {
        Some("injected") => (SteerOutcome::Injected, None),
        Some("promptRequired") => (
            SteerOutcome::TurnEnded,
            result["reason"].as_str().map(str::to_string),
        ),
        other => (
            SteerOutcome::Refused,
            Some(cap_utf8(
                &format!(
                    "unexpected steering outcome {}",
                    other.map_or_else(|| result.to_string(), |o| format!("`{o}`"))
                ),
                DETAIL_CAP,
            )),
        ),
    }
}

pub(crate) fn advice_delivered(
    key: &UnitKey,
    finding_ids: Vec<String>,
    carrier: &str,
    outcome: SteerOutcome,
    detail: Option<String>,
) -> CoreEvent {
    CoreEvent::AdviceDelivered {
        session: key.0.clone(),
        ord: key.1,
        attempt: key.2,
        finding_ids,
        carrier: carrier.to_string(),
        outcome: outcome.as_str().to_string(),
        detail,
    }
}

/// Parse the worker's `ADVICE` lines (DES §5.3):
/// `^\s*ADVICE (f-[0-9a-f]{16}): (ACCEPT|DECLINE)\b\s*[—:-]?\s*(.*)$`. The LAST line per id wins.
pub(crate) fn parse_advice_lines(output: &str) -> BTreeMap<String, AdviceResponse> {
    let mut out = BTreeMap::new();
    for line in output.lines() {
        if let Some(r) = parse_advice_line(line) {
            out.insert(r.finding_id.clone(), r);
        }
    }
    out
}

fn parse_advice_line(line: &str) -> Option<AdviceResponse> {
    let rest = line.trim_start().strip_prefix("ADVICE ")?;
    let id = rest.get(..18)?;
    let hex = id.strip_prefix("f-")?;
    if !hex
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let rest = rest[18..].strip_prefix(": ")?;
    let (disposition, rest) = match rest.strip_prefix("ACCEPT") {
        Some(r) => (Disposition::Accepted, r),
        None => (Disposition::Declined, rest.strip_prefix("DECLINE")?),
    };
    // `\b`: the keyword must not run on into a word character (`ACCEPTED` is not `ACCEPT`).
    if rest
        .chars()
        .next()
        .is_some_and(|c| c.is_alphanumeric() || c == '_')
    {
        return None;
    }
    let rest = rest.trim_start();
    let rest = rest
        .strip_prefix('—')
        .or_else(|| rest.strip_prefix(':'))
        .or_else(|| rest.strip_prefix('-'))
        .unwrap_or(rest);
    Some(AdviceResponse {
        finding_id: id.to_string(),
        disposition,
        reason: cap_utf8(rest.trim(), REASON_CAP),
    })
}

/// End of an attempt's turn, on EVERY carrier (DES §5.1, §5.3). Two things, in order:
///
/// 1. Whatever is still queued for `key` did not reach the worker mid-turn. It is recorded
///    `not_delivered` and disclosed as ONE `adviceDelivered{outcome:"not_delivered"}` naming why:
///    the carrier has no mid-turn channel, or the turn ended before the next tool-call boundary.
///    Nothing is dropped silently, and no second mechanism is tried.
/// 2. When the turn succeeded (`parse_output`), the worker's `ADVICE` lines are read for the ids
///    this attempt delivered: one `workerAdviceResponse` per answered id, and every delivered id
///    without a line is recorded `unanswered`. A failed turn has no final answer to read.
///
/// Returns the events to emit, in order.
pub(crate) fn finish_attempt(
    mailbox: &SteerMailbox,
    key: &UnitKey,
    output: Option<&str>,
) -> Vec<CoreEvent> {
    let mut events = Vec::new();
    let left = mailbox.take_queued(key);
    let steering_channel = mailbox.with_record(key, |r| r.steering_channel);
    if !left.is_empty() {
        let (carrier, detail) = if steering_channel {
            (
                CARRIER_ACP_STEERING,
                "the turn ended before another tool-call boundary; the finding goes to the gate",
            )
        } else {
            (
                CARRIER_NONE,
                "this unit's carrier has no mid-turn channel (only an ACP adapter advertising \
                 _meta.steering.supported takes a steer); the finding goes to the gate",
            )
        };
        let ids: Vec<String> = left.iter().map(|a| a.finding.finding_id.clone()).collect();
        for id in &ids {
            mailbox.record(
                key,
                id,
                Delivery::NotDelivered {
                    detail: detail.to_string(),
                },
            );
        }
        events.push(advice_delivered(
            key,
            ids,
            carrier,
            SteerOutcome::NotDelivered,
            Some(detail.to_string()),
        ));
    }
    let Some(output) = output else {
        return events;
    };
    let delivered: Vec<String> = mailbox.with_record(key, |r| {
        r.deliveries
            .iter()
            .filter(|(_, d)| **d == Delivery::Injected)
            .map(|(id, _)| id.clone())
            .collect()
    });
    if delivered.is_empty() {
        return events;
    }
    let parsed = parse_advice_lines(output);
    mailbox.with_record(key, |r| {
        r.unanswered.clear();
        for id in &delivered {
            match parsed.get(id) {
                Some(resp) => {
                    events.push(CoreEvent::WorkerAdviceResponse {
                        session: key.0.clone(),
                        ord: key.1,
                        attempt: key.2,
                        finding_id: id.clone(),
                        disposition: resp.disposition.as_str().to_string(),
                        reason: resp.reason.clone(),
                    });
                    r.responses.insert(id.clone(), resp.clone());
                }
                None => r.unanswered.push(id.clone()),
            }
        }
    });
    events
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod s2_tests;
