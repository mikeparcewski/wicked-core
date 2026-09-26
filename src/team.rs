//! Real-time teaming (DES-TEAMING-001, #590; on the bus since DES-TEAMING-002). Per DES-002 §13
//! the team's engine pieces live here and in its submodules:
//!
//! - [`events`] (T1): every `wicked.team.*` type, its payload and key, and `fold`, the pure
//!   function that turns one attempt's rows into its [`TeamLedger`];
//! - [`publish`] (P1): `TeamBus::publish`, the team outbox, the engine's publisher thread;
//! - [`runner`] (T5/T6): the attempt runner's rows — `step.claimed`, the step-boundary advice
//!   block, the PA's `ADVICE` / `HELP:` / `STEP` lines, `step.completed`, the bounded gate wait;
//! - [`supervisor`] (T6): the members' host on a bus cursor — batches, the final pass, the hold
//!   round, councils, help answers and `ledger.folded`; replay-then-tail across restarts.
//!
//! This module keeps what they share: the carrier's per-turn context ([`TeamTurn`]: a claimed
//! attempt's `checkpoint.reached` and its steer point, whose source is the attempt's
//! `finding.raised` rows), the finding grammar and its mechanical confirmation (file:line or it
//! did not happen), finding ids, line keys and anchors, the ledger types, the advice text, and the
//! team's line grammars. [`Finding`] is THE finding type (`finding.raised`, DES-002 §6 #11): the
//! supervisor constructs it, the ledger embeds it, the steer and the boundary render it. Advisory
//! throughout: nothing here denies, injects a verdict or decides — the gate does.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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
pub mod runner;
pub mod supervisor;

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

/// How many members watch an attempt and on which seat instances: the supervisor derives it
/// from the run's stream (the roster on `path.started`, the band on `plan.accepted`, the PA's
/// ask on `plan.proposed`; [`crate::team::supervisor::monitor_target`]).
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

/// The claim a teamed attempt's turn runs under (DES-002 §4.2 "Steer point"): the bus it
/// publishes on and the attempt's floor, its own `step.claimed` event id.
#[derive(Debug, Clone)]
pub(crate) struct TurnClaim {
    pub runner: runner::TeamRunner,
    pub claimed_id: i64,
    /// The seat instance the attempt runs under (`by` on its rows).
    pub by: String,
}

/// The team context of ONE unit turn on the ACP carrier — the `team: Option<&TeamTurn>`
/// parameter of `exec_turn_acp_posture` (DES-002 §4.2, §8.9). It owns the tool-call memo and the
/// checkpoint sequence (`checkpoint.reached`, R), the steer point's per-attempt delivered set, and
/// the re-confirmation root. `None` for chat turns, monitor turns and the engine's OWN sessions
/// (the agent judge, triage): they share the unit's `(run, ord, attempt)` but are not the worker,
/// so they must never take its advice nor checkpoint for it.
pub struct TeamTurn {
    /// `(run, ord, attempt)`.
    pub key: UnitKey,
    /// The attempt's claim. `None` = the attempt is not teamed (no team unit, a `transport: none`
    /// snapshot, or its `step.claimed` is not on the bus): the turn checkpoints nothing and takes
    /// no steer.
    pub(crate) claim: Option<TurnClaim>,
    /// The unit's worktree and the git dir its baseline was snapshotted through — what a fresh
    /// snapshot re-confirms a finding against before it is sent. `None` when the unit has no
    /// worktree baseline: nothing can be re-confirmed, so nothing is sent.
    pub confirm_root: Option<(PathBuf, PathBuf)>,
    /// The unit's worktree — a location under it is reported repo-relative.
    workdir: Option<PathBuf>,
    memo: Mutex<HashMap<String, ToolMemo>>,
    seq: AtomicU64,
    /// Steer point: `raise_seq`s this turn already carried or dropped (the per-attempt delivered
    /// set, DES-002 §4.2): a finding is offered to one steer at most.
    delivered: Mutex<BTreeSet<u32>>,
    steer_seq: AtomicU64,
}

impl TeamTurn {
    pub(crate) fn new(
        key: UnitKey,
        claim: Option<TurnClaim>,
        confirm_root: Option<(PathBuf, PathBuf)>,
        workdir: Option<PathBuf>,
    ) -> Self {
        Self {
            key,
            claim,
            confirm_root,
            workdir,
            memo: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(0),
            delivered: Mutex::new(BTreeSet::new()),
            steer_seq: AtomicU64::new(0),
        }
    }

    /// The unit's team context from its [`crate::workflow::StepInput`]. The attempt is teamed only
    /// on positive evidence: its snapshot says `transport: bus` AND carries its own `step.claimed`
    /// event id (set by the worker thread after the claim landed) AND this carrier has a team
    /// runner. `None` for the engine's own sessions.
    pub(crate) fn for_unit(
        input: &crate::workflow::StepInput,
        runner: Option<&runner::TeamRunner>,
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
        let claim = match (input.unit.team.as_ref(), runner) {
            (Some(t), Some(r)) if t.transport == events::Transport::Bus => {
                t.claimed_event_id.map(|claimed_id| TurnClaim {
                    runner: r.clone(),
                    claimed_id,
                    by: runner::seat_of(input),
                })
            }
            _ => None,
        };
        Some(Self::new(
            (input.run_id.clone(), input.unit.ord, input.attempt),
            claim,
            confirm_root,
            input.workdir.clone(),
        ))
    }

    /// Whether this turn's attempt is teamed (its `step.claimed` is on the bus).
    pub fn teamed(&self) -> bool {
        self.claim.is_some()
    }

    fn relative(&self, p: &str) -> String {
        if let Some(root) = self.workdir.as_deref() {
            if let Ok(rel) = Path::new(p).strip_prefix(root) {
                return rel.to_string_lossy().replace('\\', "/");
            }
        }
        p.to_string()
    }

    fn envelope(&self, re: Option<String>) -> events::Envelope {
        events::Envelope {
            run_id: self.key.0.clone(),
            ord: Some(self.key.1),
            attempt: Some(self.key.2),
            by: self
                .claim
                .as_ref()
                .map_or_else(|| "claude".to_string(), |c| c.by.clone()),
            at: crate::interaction::now_millis(),
            re,
        }
    }

    /// Observe one agent `session/update` frame of a TEAMED turn (an unteamed one yields `None`
    /// for every frame). `kind`, `title` and `locations` are REMEMBERED per `toolCallId` from the
    /// `tool_call` and refinement frames, because the terminal `tool_call_update` does not repeat
    /// them; that terminal frame (`status` `completed` | `failed`) yields exactly one
    /// `checkpoint.reached` (DES-002 §6 #10). Every other frame — message chunks, usage, a
    /// non-terminal update — yields `None`: checkpoints are tool-call boundaries, never tokens.
    pub fn observe(&self, frame: &Value) -> Option<events::TeamEvent> {
        self.claim.as_ref()?;
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
        let status = match (which, update["status"].as_str()) {
            ("tool_call_update", Some("completed")) => events::CheckpointStatus::Completed,
            ("tool_call_update", Some("failed")) => events::CheckpointStatus::Failed,
            _ => return None,
        };
        let done = memo.remove(&id).unwrap_or_default();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        Some(events::TeamEvent {
            env: self.envelope(None),
            body: events::TeamBody::CheckpointReached(events::CheckpointReached {
                seq,
                tool_call_id: id,
                kind: done.kind.unwrap_or_else(|| "other".to_string()),
                title: cap_utf8(done.title.as_deref().unwrap_or(""), TITLE_CAP),
                status,
                paths: done.paths,
            }),
        })
    }

    /// Publish one of this attempt's R facts (a checkpoint, a steer's `advice.delivered`) through
    /// the one wrapper. A spooled or refused fact is logged: a checkpoint only paces the members'
    /// batches (the final pass reviews the settled tree whatever arrived), and an `advice.delivered`
    /// that never lands leaves its finding undelivered, which the boundary re-renders and the gate
    /// treats as unresolved — never as delivered.
    pub(crate) fn publish(&self, ev: &events::TeamEvent) {
        let Some(c) = self.claim.as_ref() else {
            return;
        };
        match c.runner.bus().publish(ev) {
            Ok(publish::PublishOutcome::Published(_)) => {}
            Ok(other) => eprintln!(
                "wicked-core: {} of {}:{}:{} not on the bus ({other:?})",
                ev.event_type(),
                self.key.0,
                self.key.1,
                self.key.2
            ),
            Err(e) => eprintln!(
                "wicked-core: {} of {}:{}:{} not written ({e:#})",
                ev.event_type(),
                self.key.0,
                self.key.1,
                self.key.2
            ),
        }
    }

    /// The steer point's source (DES-002 §8.9): every `finding.raised{severity:"high"}` of THIS
    /// attempt after its own `step.claimed`, minus the findings this turn already offered. A
    /// stream it cannot read yields nothing (the boundary and the gate still see the rows).
    pub(crate) fn pending_high(&self) -> Vec<(u32, Finding)> {
        let Some(c) = self.claim.as_ref() else {
            return Vec::new();
        };
        let stream = match c.runner.read_run(&self.key.0, c.claimed_id) {
            Ok(s) => s,
            Err(e) => {
                eprintln!(
                    "wicked-core: steer point of {}:{}:{} could not read the stream ({e:#})",
                    self.key.0, self.key.1, self.key.2
                );
                return Vec::new();
            }
        };
        let offered = self.delivered.lock().unwrap_or_else(|p| p.into_inner());
        let mut out: Vec<(u32, Finding)> = Vec::new();
        for row in stream.rows {
            let env = &row.event.env;
            if env.ord != Some(self.key.1) || env.attempt != Some(self.key.2) {
                continue;
            }
            let events::TeamBody::FindingRaised(b) = &row.event.body else {
                continue;
            };
            if b.severity != Severity::High
                || offered.contains(&b.raise_seq)
                || out.iter().any(|(s, _)| *s == b.raise_seq)
            {
                continue;
            }
            out.push((b.raise_seq, runner::finding_of(&row.event.env, b)));
        }
        out
    }

    /// Record that this turn offered `raise_seq` to a steer (sent, dropped as superseded, or
    /// disclosed as not delivered): it is never offered again in this turn.
    pub(crate) fn mark_offered(&self, raise_seq: u32) {
        self.delivered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(raise_seq);
    }

    /// Offer `raise_seq` again at the next boundary (it did not fit this steer's block).
    pub(crate) fn unmark_offered(&self, raise_seq: u32) {
        self.delivered
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&raise_seq);
    }

    /// A fresh steer id for this attempt (`advice.delivered.delivery_id` / `steer_id`): the
    /// carrier's per-attempt counter, never content (DES-002 §6.1 row 12).
    pub(crate) fn next_steer_id(&self) -> String {
        let n = self.steer_seq.fetch_add(1, Ordering::Relaxed) + 1;
        format!(
            "s-{}",
            crate::bus::deterministic_key(&[
                &self.key.0,
                &self.key.1.to_string(),
                &self.key.2.to_string(),
                &n.to_string(),
            ])
        )
    }

    /// One `advice.delivered{channel:"acp_steering"}` row per finding a steer carried (DES-002
    /// §8.9), all sharing the steer's id.
    pub(crate) fn steered(
        &self,
        steer_id: &str,
        carried: &[(u32, String)],
        outcome: events::DeliveryOutcome,
        detail: Option<String>,
    ) {
        for (raise_seq, finding_id) in carried {
            self.publish(&events::TeamEvent {
                env: self.envelope(Some(format!("finding.raised#{raise_seq}"))),
                body: events::TeamBody::AdviceDelivered(events::AdviceDelivered {
                    raise_seq: *raise_seq,
                    finding_id: finding_id.clone(),
                    delivery_id: steer_id.to_string(),
                    steer_id: Some(steer_id.to_string()),
                    channel: events::Channel::AcpSteering,
                    outcome,
                    detail: detail.clone(),
                }),
            });
        }
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

/// The finding id (DES-001 §4.6 step 4): `f-` + the first 16 hex of sha256(`path` ‖ `\n` ‖
/// `anchor` ‖ `\n` ‖ normalized `evidence`). Keyed on the line's TEXT and its enclosing location,
/// never its number, so an edit that shifts lines mints no new finding, and the same hazardous
/// line in two functions of one file is two findings.
///
/// A file-level finding (`anchor == ""`) keeps the pre-anchor spelling, sha256(`path` ‖ `\n` ‖
/// normalized `evidence`), so every id minted before T6 stays the same id: two findings in one
/// file with different anchors still differ from each other and from the file-level one.
pub fn finding_id_anchored(path: &str, anchor: &str, evidence: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(b"\n");
    if !anchor.is_empty() {
        h.update(anchor.as_bytes());
        h.update(b"\n");
    }
    h.update(normalize_evidence(evidence).as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("f-{hex}")
}

/// The moved-line correlation key (DES-001 §4.6 `lineKey`): `l-` + the first 16 hex of
/// sha256(`path` ‖ `\n` ‖ normalized `evidence`). Used only to find a finding's line again at
/// `T_final`; it never merges findings.
pub fn line_key(path: &str, evidence: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(path.as_bytes());
    h.update(b"\n");
    h.update(normalize_evidence(evidence).as_bytes());
    let digest = h.finalize();
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    format!("l-{hex}")
}

/// The longest anchor a finding carries.
pub const ANCHOR_CAP: usize = 160;

/// The enclosing location of line `line` in `file` (DES-001 §4.6 step 4, anchor source (ii)):
/// git's default funcname heuristic, the nearest line ABOVE `line` that starts with a letter,
/// `_` or `$` (the text git prints after a hunk header's `@@`). `""` when there is none (a new
/// file of top-level statements), which is a file-level finding.
pub fn anchor_of(file: Option<&str>, line: u32) -> String {
    let Some(file) = file else {
        return String::new();
    };
    let upto = line.saturating_sub(1) as usize;
    file.lines()
        .take(upto)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .find(|l| {
            l.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
        })
        .map(|l| cap_utf8(l.trim_end(), ANCHOR_CAP))
        .unwrap_or_default()
}

/// Whether line `line` (1-based) of `file` IS `evidence` — the exact text (DES §4.6 step 3,
/// "file:line or it did not happen": the monitor prompt asks for the exact line and
/// `finding.raised.evidence` is documented as equal to it). Whitespace normalization serves the
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
    /// The finding as it was confirmed and raised (`finding.raised`), flattened.
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

/// (T6, DES-002 §8.8) The PA's review of a member's step, as the attempt that carried the review
/// recorded it: the `STEP` verdict, and for a rejection whether the member held its output and
/// what the one-off council ruled. The engine reads it to decide whether the member's step counts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepReviewRecord {
    pub step_id: String,
    /// The member attempt the review is about.
    pub reviewed_attempt: u32,
    pub verdict: events::StepVerdict,
    pub to: Option<events::ReworkBy>,
    pub reason: String,
    /// On a rejection: `Some(true)` the member held (`HOLD`), `Some(false)` it accepted the
    /// rejection, `None` no answer on record — which is never read as either (the engine pauses).
    pub held: Option<bool>,
    pub member_reason: Option<String>,
    /// The council a hold convened (`council.ruled` on subject `step:<id>:<attempt>`).
    pub dispute: Option<Dispute>,
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
    /// (T6) The PA's reviews of member steps this attempt carried (DES-002 §8.8).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub step_reviews: Vec<StepReviewRecord>,
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
            step_reviews: Vec::new(),
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
    #[serde(default)]
    step_reviews: Vec<StepReviewRecord>,
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
            step_reviews: w.step_reviews,
        }
    }
}

/// What [`FindingBook::admit`] did with a confirmed, above-bar finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Admit {
    /// First seen: publish it as `finding.raised` with this `raise_seq`.
    New(Box<LedgerFinding>, u32),
    /// Another monitor already raised it: recorded in `corroboratedBy`, not re-raised.
    Corroborated,
    /// The same monitor repeated it: counted as `duplicate`, not re-raised.
    Duplicate,
}

/// The attempt's findings, deduplicated by id (DES §4.6 step 4), with the supervisor's
/// per-attempt emission counter (`raise_seq`, the `finding.raised` key, DES-002 §6.1 row 11).
#[derive(Debug, Default)]
pub struct FindingBook {
    pub findings: Vec<LedgerFinding>,
    /// `raise_seq` of `findings[i]`.
    pub raises: Vec<u32>,
    index: HashMap<String, usize>,
    pub rejected: Rejected,
    next_raise: u32,
}

impl FindingBook {
    /// Admit one confirmed, above-bar finding. Its `finding_id` is minted here from `path`,
    /// `anchor` and `evidence` (whatever the caller put there is replaced), so dedup and the id
    /// agree by construction.
    pub fn admit(&mut self, mut finding: Finding) -> Admit {
        let id = finding_id_anchored(&finding.path, &finding.anchor, &finding.evidence);
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
        self.next_raise += 1;
        let seq = self.next_raise;
        self.index.insert(id, self.findings.len());
        self.findings.push(f.clone());
        self.raises.push(seq);
        Admit::New(Box::new(f), seq)
    }

    /// The `raise_seq` of the finding `finding_id`, if this attempt raised it.
    pub fn raise_of(&self, finding_id: &str) -> Option<u32> {
        self.index.get(finding_id).map(|&i| self.raises[i])
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

// ── The attempt a supervisor watches (DES-002 §8.8) ─────────────────────────────────────────────

/// `(run, ord, attempt)`.
pub type UnitKey = (String, u32, u32);

/// One attempt, as the supervisor reads it off its `step.claimed` row (DES-002 §6 #9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachCtx {
    pub run_id: String,
    pub ord: u32,
    pub attempt: u32,
    /// The creator's seat instance (the `step.claimed` `by`) — never a monitor of its own step.
    pub creator: String,
    pub plan: TeamPlan,
    /// `None` when the unit is unbound or its dispatch baseline was not taken: not monitored,
    /// and said so.
    pub repo: Option<Repo>,
    /// The unit's dispatch baseline tree.
    pub baseline_tree: Option<String>,
    pub criterion: String,
    pub phase: String,
    pub step_id: String,
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

/// The carrier a member session runs on. Production: `AcpStepRunner` (`monitor_ensure` /
/// `monitor_turn` beside `chat_turn`, the chat boundary, no chat events). Tests: a fake.
pub trait MonitorHost: Send + Sync {
    /// DES §4.1 (b): the seat's `[cli.acp]` is admitted to input governance — the read-only
    /// boundary is enforced by answering `session/request_permission`, which an unadmitted
    /// adapter never sends.
    fn admitted(&self, seat: &str) -> Result<(), String>;
    /// Start (or reuse) the member session `pool_key` on `seat`, read-only over `scope`.
    fn open(&self, pool_key: &str, seat: &str, scope: &MonitorScope) -> Result<(), String>;
    /// One member turn; the reply text. `Err` means the turn failed AND the host closed the
    /// session (`AcpStepRunner::monitor_turn` evicts on any failure): the supervisor reopens it
    /// on the next batch through [`MonitorHost::open`].
    fn turn(&self, pool_key: &str, prompt: &str, budget: Duration) -> Result<String, String>;
    /// Close the session.
    fn close(&self, pool_key: &str);
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

/// A confirmed member finding, as `finding.raised` carries it: THE one finding type.
/// The supervisor's confirmation constructs it, the ledger embeds it ([`LedgerFinding`]), and the
/// steer point and the step boundary render it.
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
    /// The last checkpoint the batch that raised it covered (the `re` of its `finding.raised`).
    /// Not part of the ledger's finding (DES §7 `teamLedger.findings[]`).
    #[serde(skip)]
    pub checkpoint_seq: u64,
    /// (T6) The enclosing location the id is minted over ([`anchor_of`]); `""` = file-level.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub anchor: String,
    /// (T6) The attempt that first raised it, when it was carried into a redriven attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_from_attempt: Option<u32>,
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
    #[serde(default)]
    anchor: String,
    #[serde(default)]
    carried_from_attempt: Option<u32>,
}

impl From<FindingWire> for Finding {
    fn from(w: FindingWire) -> Self {
        Finding {
            finding_id: finding_id_anchored(&w.path, &w.anchor, &w.evidence),
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
            anchor: w.anchor,
            carried_from_attempt: w.carried_from_attempt,
        }
    }
}

/// One piece of advice: a raised finding rendered into a steer or a step-boundary block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Advice {
    pub finding: Finding,
}

/// The worker's disposition on one delivered finding (DES §5.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    Accepted,
    Declined,
}

/// One parsed `ADVICE` line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AdviceResponse {
    pub finding_id: String,
    pub disposition: Disposition,
    /// May be `""`: a refusal with no evidence is recorded as exactly that (DES §5.3).
    pub reason: String,
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
        "- {} [{}] {}:{} — {}{}\n  Evidence (that line): `{}`\n",
        f.finding_id,
        f.severity.as_str().to_ascii_uppercase(),
        f.path,
        f.line,
        f.claim,
        f.carried_from_attempt
            .map(|n| format!(" (carried_from_attempt:{n}: raised on an earlier attempt of this step that did not finish)"))
            .unwrap_or_default(),
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

/// The adapter's answer to a steering request → the `advice.delivered` outcome and detail
/// (DES §5.2): `{"outcome":"injected"}` → `injected`; `{"outcome":"promptRequired"}` →
/// `turn_ended`; a JSON-RPC error → `refused` with the error. Any other outcome (e.g.
/// `startedNewTurn`, which `promptRequired` exists to prevent) is `refused` and named.
pub(crate) fn classify_steer_answer(
    v: &serde_json::Value,
) -> (events::DeliveryOutcome, Option<String>) {
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
        return (
            events::DeliveryOutcome::Refused,
            Some(cap_utf8(&detail, DETAIL_CAP)),
        );
    }
    let result = &v["result"];
    match result["outcome"].as_str() {
        Some("injected") => (events::DeliveryOutcome::Injected, None),
        Some("promptRequired") => (
            events::DeliveryOutcome::TurnEnded,
            result["reason"].as_str().map(str::to_string),
        ),
        other => (
            events::DeliveryOutcome::Refused,
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

// ── DES-TEAMING-002 T6: the team's line grammars (§8.8) ─────────────────────────────────────────

/// A `HELP:` question or its context is capped at 4 KB (DES-002 §6 #14).
pub(crate) const HELP_CAP: usize = 4 * 1024;

/// At most this many `HELP:` lines are taken from one step's output.
pub(crate) const HELP_MAX: usize = 8;

/// The PA's `HELP: <question>` lines, in order (at most [`HELP_MAX`], each capped). An empty
/// question is not a question.
pub(crate) fn parse_help_lines(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("HELP:"))
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .take(HELP_MAX)
        .map(|q| cap_utf8(q, HELP_CAP))
        .collect()
}

/// One parsed `STEP` line: the PA's review of a member's step (DES-002 §8.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepLine {
    pub step_id: String,
    pub verdict: events::StepVerdict,
    pub to: Option<events::ReworkBy>,
    pub reason: String,
}

/// `s` with a leading `—`, `:` or `-` separator and surrounding whitespace removed.
fn after_separator(s: &str) -> &str {
    let s = s.trim_start();
    s.strip_prefix('—')
        .or_else(|| s.strip_prefix(':'))
        .or_else(|| s.strip_prefix('-'))
        .unwrap_or(s)
        .trim()
}

/// The PA's `STEP <step_id>: ACCEPT — <reason>` / `STEP <step_id>: REJECT to:member|to:pa — <reason>`
/// lines. The FIRST line per step id wins (DES-002 §6.1 row 18). A `REJECT` naming no target
/// goes back to the member (the first rework); a malformed line is not a review.
pub(crate) fn parse_step_lines(output: &str) -> Vec<StepLine> {
    let mut out: Vec<StepLine> = Vec::new();
    for line in output.lines() {
        let Some(rest) = line.trim_start().strip_prefix("STEP ") else {
            continue;
        };
        let Some((id, rest)) = rest.split_once(':') else {
            continue;
        };
        let id = id.trim();
        if id.is_empty() || id.contains(char::is_whitespace) || out.iter().any(|l| l.step_id == id)
        {
            continue;
        }
        let rest = rest.trim_start();
        let (verdict, rest) = if let Some(r) = rest.strip_prefix("ACCEPT") {
            (events::StepVerdict::Accepted, r)
        } else if let Some(r) = rest.strip_prefix("REJECT") {
            (events::StepVerdict::Rejected, r)
        } else {
            continue;
        };
        if rest
            .chars()
            .next()
            .is_some_and(|c| c.is_alphanumeric() || c == '_')
        {
            continue;
        }
        let mut rest = rest.trim_start();
        let mut to = None;
        if verdict == events::StepVerdict::Rejected {
            to = Some(events::ReworkBy::Member);
            for (tok, who) in [
                ("to:member", events::ReworkBy::Member),
                ("to:pa", events::ReworkBy::Pa),
                ("member", events::ReworkBy::Member),
                ("pa", events::ReworkBy::Pa),
            ] {
                if let Some(r) = rest.strip_prefix(tok) {
                    if !r.chars().next().is_some_and(|c| c.is_alphanumeric()) {
                        to = Some(who);
                        rest = r;
                        break;
                    }
                }
            }
        }
        out.push(StepLine {
            step_id: id.to_string(),
            verdict,
            to,
            reason: cap_utf8(after_separator(rest), REASON_CAP),
        });
    }
    out
}

/// A member's hold-round answers (DES-001 §4.5): `HOLD <findingId> — <reason>` or
/// `WITHDRAW <findingId> — <reason>`, the LAST line per id wins. Anything else is ignored; an id
/// with no line is a HOLD by the caller's rule (a member's silence never clears a finding).
pub(crate) fn parse_hold_lines(reply: &str) -> BTreeMap<String, MonitorReply> {
    let mut out = BTreeMap::new();
    for line in reply.lines() {
        let line = line.trim_start();
        let (kind, rest) = if let Some(r) = line.strip_prefix("HOLD ") {
            (ReplyKind::Hold, r)
        } else if let Some(r) = line.strip_prefix("WITHDRAW ") {
            (ReplyKind::Withdraw, r)
        } else {
            continue;
        };
        let rest = rest.trim_start();
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        if !id.starts_with("f-") || id.len() != 18 {
            continue;
        }
        out.insert(
            id.clone(),
            MonitorReply {
                kind,
                reason: cap_utf8(after_separator(&rest[id.len()..]), REASON_CAP),
            },
        );
    }
    out
}

/// A member's answer to the PA's rejection of its step (DES-002 §8.8): `HOLD <step_id> — <reason>`
/// holds its output (a concrete dispute: the council path); `ACCEPT <step_id>` takes the
/// rejection. `None` = neither line for this step (no answer on record).
pub(crate) fn parse_member_step_answer(reply: &str, step_id: &str) -> Option<(bool, String)> {
    for line in reply.lines() {
        let line = line.trim_start();
        let (held, rest) = if let Some(r) = line.strip_prefix("HOLD ") {
            (true, r)
        } else if let Some(r) = line.strip_prefix("ACCEPT ") {
            (false, r)
        } else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(tail) = rest.strip_prefix(step_id) else {
            continue;
        };
        // The id ends at whitespace, `:` or the line's end: `build-2` is not `build`.
        if tail
            .chars()
            .next()
            .is_some_and(|c| !c.is_whitespace() && c != ':')
        {
            continue;
        }
        return Some((held, cap_utf8(after_separator(tail), REASON_CAP)));
    }
    None
}

/// A member's answer to a `HELP:` question: every line but `EVIDENCE:` lines and a final `DONE`
/// (capped at 4 KB), and the `EVIDENCE: <path:line>` citations (at most 16).
pub(crate) fn parse_help_answer(reply: &str) -> (String, Vec<String>) {
    let mut answer = Vec::new();
    let mut evidence = Vec::new();
    for line in reply.lines() {
        let t = line.trim();
        if let Some(e) = t.strip_prefix("EVIDENCE:") {
            let e = e.trim();
            if !e.is_empty() && evidence.len() < 16 {
                evidence.push(cap_utf8(e, 512));
            }
        } else if t != "DONE" {
            answer.push(t.strip_prefix("ANSWER:").map(str::trim).unwrap_or(t));
        }
    }
    (cap_utf8(answer.join("\n").trim(), HELP_CAP), evidence)
}

/// A member's `CHANGE {"steps":[…],"reason":"…"}` lines in a batch reply: a request for plan
/// steps (DES-002 §8.7, `change.requested`). A line whose JSON does not parse into steps is
/// ignored (it is not a request).
pub(crate) fn parse_change_lines(reply: &str) -> Vec<(Vec<events::PlanStep>, String)> {
    #[derive(Deserialize)]
    struct Change {
        steps: Vec<events::PlanStep>,
        #[serde(default)]
        reason: String,
    }
    reply
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("CHANGE "))
        .filter_map(|j| serde_json::from_str::<Change>(j.trim()).ok())
        .filter(|c| !c.steps.is_empty())
        .map(|c| (c.steps, cap_utf8(&c.reason, 4 * 1024)))
        .collect()
}

#[cfg(test)]
mod tests;
