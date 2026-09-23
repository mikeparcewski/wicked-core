//! Real-time teaming (DES-TEAMING-001, #590).
//!
//! This file carries **S3 — monitor→worker injection** (#602, DES §5): the steer mailbox the
//! supervisor writes HIGH findings into, the advice text a steer carries, the per-attempt
//! delivery record, the carrier-independent "not delivered mid-turn" disclosure, and the
//! worker's `ADVICE <id>: ACCEPT|DECLINE — <reason>` parsing.
//!
//! S2 (#601) owns the `TeamSupervisor`, `TeamCmd`, confirmation, dedup and the final pass, and
//! S6 (#603) owns the `TeamLedger`; both live in this file per DES §9. [`Finding`] below is the
//! MINIMAL struct S3 needs, shaped exactly as the DES's `monitorFinding` (§7): S2 defines the
//! finding, and whichever of the two PRs lands second converges on the one type.
//!
//! The one mid-turn carrier is the ACP adapter's `_session/steering` (claude-agent-acp 0.73.0,
//! `dist/acp-agent.js:1186-1272`). Every steer carries `idleBehavior: "promptRequired"`, so a
//! steer that lands after the turn settled returns `promptRequired` instead of starting a
//! detached turn (`acp-agent.js:1228-1242`). No other carrier takes a steer, and none gets a
//! second mechanism (DES §5.1): whatever is still queued when an attempt's turn ends is recorded
//! as not delivered mid-turn, with the reason, and goes to the gate.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::event::CoreEvent;

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

/// The key every piece of S3 state is scoped by: `(run, ord, attempt)` (DES §5.2). Keying by
/// attempt means advice for a superseded attempt can never reach its successor.
pub(crate) type AdviceKey = (String, u32, u32);

/// A finding's severity. Only `high` is ever steered into the worker (DES §4.6 step 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Severity {
    High,
    // Constructed by S2's confirmation (#601); S3 only ever refuses it.
    #[cfg_attr(not(test), allow(dead_code))]
    Medium,
}

impl Severity {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Severity::High => "high",
            Severity::Medium => "medium",
        }
    }
}

/// A confirmed monitor finding, as `monitorFinding` shapes it (DES §7). MINIMAL: S2 (#601) owns
/// this type; S3 reads only what a steer and its re-confirmation need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
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
    pub checkpoint_seq: u64,
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
    queued: HashMap<AdviceKey, Vec<Advice>>,
    records: HashMap<AdviceKey, AttemptAdvice>,
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
    pub(crate) fn queue(&self, key: AdviceKey, finding: Finding) -> bool {
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
    pub(crate) fn take_queued(&self, key: &AdviceKey) -> Vec<Advice> {
        self.lock().queued.remove(key).unwrap_or_default()
    }

    /// Put advice back at the FRONT of `key`'s queue (what did not fit the 8 KB block).
    pub(crate) fn requeue_front(&self, key: &AdviceKey, advice: Vec<Advice>) {
        if advice.is_empty() {
            return;
        }
        let mut g = self.lock();
        let q = g.queued.entry(key.clone()).or_default();
        let rest = std::mem::take(q);
        q.extend(advice);
        q.extend(rest);
    }

    pub(crate) fn record(&self, key: &AdviceKey, finding_id: &str, delivery: Delivery) {
        self.lock()
            .records
            .entry(key.clone())
            .or_default()
            .deliveries
            .insert(finding_id.to_string(), delivery);
    }

    pub(crate) fn mark_steering_channel(&self, key: &AdviceKey) {
        self.lock()
            .records
            .entry(key.clone())
            .or_default()
            .steering_channel = true;
    }

    fn with_record<T>(&self, key: &AdviceKey, f: impl FnOnce(&mut AttemptAdvice) -> T) -> T {
        f(self.lock().records.entry(key.clone()).or_default())
    }

    /// Read a copy of `key`'s record without consuming it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn record_of(&self, key: &AdviceKey) -> Option<AttemptAdvice> {
        self.lock().records.get(key).cloned()
    }

    /// Hand `key`'s record to its consumer (S2's final pass / S6's ledger) and forget it.
    // Read by S2's final pass (#601); until it lands only the tests call it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn take_record(&self, key: &AdviceKey) -> Option<AttemptAdvice> {
        self.lock().records.remove(key)
    }

    /// Forget every attempt of `run_id` (called when the run completes).
    pub(crate) fn prune_run(&self, run_id: &str) {
        let mut g = self.lock();
        g.queued.retain(|(r, _, _), _| r != run_id);
        g.records.retain(|(r, _, _), _| r != run_id);
    }
}

/// The team parameter `exec_turn_acp_posture` gains (DES §5.2). `None` for chat turns and for
/// every test that does not exercise teaming. S2 adds the tool-call memo (DES §4.2) here.
pub(crate) struct TeamTurn {
    pub key: AdviceKey,
    pub mailbox: SteerMailbox,
    /// The unit's worktree and the git dir its baseline was snapshotted through — what a fresh
    /// snapshot re-confirms a finding against before it is sent. `None` when the unit has no
    /// worktree baseline: nothing can be re-confirmed, so nothing is sent.
    pub confirm_root: Option<(PathBuf, PathBuf)>,
}

impl TeamTurn {
    /// The unit's team context from its [`crate::workflow::StepInput`]. `None` for the engine's
    /// OWN sessions (the agent judge, triage): they share the unit's `(run, ord, attempt)` but are
    /// not the worker, so they must never drain its advice nor answer it.
    pub(crate) fn for_unit(
        input: &crate::workflow::StepInput,
        mailbox: &SteerMailbox,
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
        Some(TeamTurn {
            key: (input.run_id.clone(), input.unit.ord, input.attempt),
            mailbox: mailbox.clone(),
            confirm_root,
        })
    }
}

/// Whitespace-trimmed and -collapsed, the comparison form of an evidence line (DES §4.6 step 3).
pub(crate) fn normalize_evidence(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `cap` bytes at a UTF-8 boundary.
pub(crate) fn cap_utf8(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        return s.to_string();
    }
    let mut end = cap;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
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
    key: &AdviceKey,
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
    key: &AdviceKey,
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
