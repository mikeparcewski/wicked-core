//! The worker-thread seam (DES-TEAMING-002 §4.2, §8.9, §8.11, seam T5): the attempt runner's
//! `step.claimed` / `step.completed`, the step-boundary injector, and the bounded gate wait.
//!
//! **One mechanism.** Every fact here is an R fact published through [`TeamBus::publish`] — the
//! same wrapper, outbox, lane FIFO and supersede rule as the engine's publisher (P1). The worker
//! thread never opens a bus of its own: it reaches the process-wide [`BusDb::shared`] handle
//! through `TeamBus` for writes and through [`TeamRunner::read_run`] for its two bounded reads (the
//! injector's one read per step, the gate wait's poll), exactly as §4.2 places them. The actor
//! never runs any of this.
//!
//! **Fail closed on absence.**
//! - `step.claimed` is required (§4.1, row 5): past the bound the attempt's runner lane is
//!   tombstoned FIRST, then the attempt is un-teamed (`transport: none`, reason). If the tombstone
//!   cannot be written the step FAILS before its turn starts — a spooled `step.claimed` must never
//!   land for an attempt that ran un-teamed.
//! - The gate wait's timeout synthesizes DES-001 §4.7's fail-closed ledger from the attempt's own
//!   rows, never published (`ledger.folded` has one owner, S). A stream it cannot read, or a gap in
//!   it, is `stream_gap`, which pauses.
//! - A snapshot stamped `bus` whose worker returned nothing is merged by the actor as a
//!   `stream_gap` ledger ([`merge_snapshot`]), which pauses.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::events::{
    self as tev, AdviceDelivered, Channel, DeliveryOutcome, Envelope, LedgerSource, StepClaimed,
    StepCompleted, StepCompletion, TeamBody, TeamEvent, TeamRow, Transcript, TranscriptRow,
    Transport,
};
use super::publish::{PublishOutcome, TeamBus, TeamConfig};
use super::{cap_utf8, FinalPass, Finding, TeamLedger};
use crate::bus::BusDb;
use crate::domain::UnitTeamSnapshot;
use crate::workflow::{PriorUnitOutput, StepInput, StepOutput, StepStatus};

/// The least the production gate wait waits for S's fold, whatever `WICKED_TEAM_FINAL_PASS_SECS`
/// says: a supplied `0` must never let a unit skip its gate wait (tests inject their own bound
/// through [`TeamConfig::with_final_pass_budget`]).
pub const MIN_GATE_WAIT: Duration = Duration::from_secs(30);

/// How often the gate wait polls for `ledger.folded` (injectable: [`TeamConfig::with_gate_poll`]).
pub const GATE_POLL: Duration = Duration::from_millis(250);

/// The transcript a snapshot carries (§6 `ledger.folded`): ≤256 KB of rows.
pub const TRANSCRIPT_CAP: usize = 256 * 1024;

/// The compact transcript `render_for_gate` gives the reviewers (§8.11): ≤16 KB.
pub const GATE_TRANSCRIPT_CAP: usize = 16 * 1024;

/// One transcript row's payload in the gate render, capped.
const GATE_ROW_CAP: usize = 1024;

/// The label of the step-boundary advice block (§8.9, acceptance T5 (b)).
pub const ADVICE_LABEL: &str = "[team advice]";

/// The bus filter every team read uses.
const TEAM_FILTER: &str = "wicked.team.**";

/// Rows per bus read.
const READ_BATCH: usize = 500;

/// The attempt runner's handle on team publishing: the run's bus and outbox (the engine's own,
/// from its [`TeamConfig`]), the required-fact bound, the final-pass budget and the gate poll.
/// Built once per engine and handed to every worker thread (in-process and the bus worker).
#[derive(Debug, Clone)]
pub struct TeamRunner {
    bus: TeamBus,
    bus_db: String,
    schedule: Vec<Duration>,
    final_pass_budget: Duration,
    gate_poll: Duration,
}

impl TeamRunner {
    /// The runner for `cfg`, or `None` when the engine has no bus or no outbox (then every team
    /// run is un-teamed at the actor already, §4.8 row 6).
    pub fn from_config(cfg: &TeamConfig) -> Option<Self> {
        let bus_db = cfg.bus_db.clone()?;
        let outbox = cfg.outbox.clone()?;
        Some(Self {
            bus: TeamBus::new(bus_db.clone(), outbox, cfg.attempt_wait),
            bus_db,
            schedule: if cfg.schedule.is_empty() {
                super::publish::RETRY_SCHEDULE.to_vec()
            } else {
                cfg.schedule.clone()
            },
            final_pass_budget: cfg.final_pass_budget,
            gate_poll: cfg.gate_poll,
        })
    }

    /// The one publish wrapper this runner writes through.
    pub fn bus(&self) -> &TeamBus {
        &self.bus
    }

    /// Every `wicked.team.*` row of `run_id` after `after`, in `event_id` order. Rows of the run
    /// that do not parse are counted, never applied (`malformed`).
    pub fn read_run(&self, run_id: &str, after: i64) -> anyhow::Result<RunStream> {
        let db = BusDb::shared(&self.bus_db)?;
        let mut floor = after;
        let mut out = RunStream::default();
        loop {
            let batch = db.poll(TEAM_FILTER, floor, READ_BATCH)?;
            let n = batch.len();
            for ev in batch {
                floor = floor.max(ev.event_id);
                if ev.payload.get("run_id").and_then(Value::as_str) != Some(run_id) {
                    continue;
                }
                match TeamEvent::from_payload(&ev.event_type, &ev.payload) {
                    Ok(event) => out.rows.push(TeamRow {
                        event_id: ev.event_id,
                        event,
                    }),
                    Err(_) => out.malformed += 1,
                }
            }
            if n < READ_BATCH {
                return Ok(out);
            }
        }
    }
}

/// One read of a run's stream.
#[derive(Debug, Clone, Default)]
pub struct RunStream {
    pub rows: Vec<TeamRow>,
    /// Rows of the run that did not parse (never applied; a gap for the fold).
    pub malformed: usize,
}

/// The finding a `finding.raised` row describes (the one construction, for the steer point, the
/// boundary and the supervisor's carry).
pub(crate) fn finding_of(env: &Envelope, b: &tev::FindingRaised) -> Finding {
    Finding {
        finding_id: b.finding_id.clone(),
        monitor_id: b.member_id.clone(),
        seat: env.by.clone(),
        severity: b.severity,
        path: b.path.clone(),
        line: b.line,
        evidence: b.evidence.clone(),
        claim: b.claim.clone(),
        suggestion: b.suggestion.clone(),
        tree: b.tree.clone(),
        in_diff: b.in_diff,
        checkpoint_seq: 0,
        anchor: b.anchor.clone().unwrap_or_default(),
        carried_from_attempt: b.carried_from_attempt,
    }
}

/// What the worker thread knows about its attempt's team, after [`claim`].
#[derive(Debug, Clone)]
pub enum Attempt {
    /// Not a team unit: nothing is published and no snapshot is built.
    NotTeam,
    /// A team unit that runs un-teamed: the run is `transport: none` (no bus, a failed
    /// `path.started`), or this attempt's `step.claimed` failed. The snapshot is built locally and
    /// never published (§4.1, §4.8 rows 1/5/6).
    Local(Box<UnitTeamSnapshot>),
    /// The attempt's `step.claimed` is on the bus.
    Claimed(Box<Claimed>),
}

/// A claimed attempt: everything the injector and the gate wait need.
#[derive(Debug, Clone)]
pub struct Claimed {
    pub runner: TeamRunner,
    pub run_id: String,
    pub ord: u32,
    pub attempt: u32,
    pub by: String,
    pub step_id: String,
    /// The run's `path.started` id (the injector's floor).
    pub stream_floor: i64,
    /// This attempt's `step.claimed` id (the attempt's floor).
    pub claimed_id: i64,
    /// (T6) The member attempt this attempt reviews, when it is the PA's review of a member's
    /// step (§8.8): its `STEP` line becomes `step.reviewed`.
    pub reviewing: Option<u32>,
    /// The step's criterion (the `HELP:` context).
    pub criterion: String,
}

/// The seat instance a step runs under (`by` on R rows): the unit's assigned seat.
pub(crate) fn seat_of(input: &StepInput) -> String {
    input
        .unit
        .assigned_cli
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .unwrap_or("claude")
        .to_string()
}

/// The plan step a unit carries: its phase id (the composed plan's step id), else `unit-<ord>`.
pub fn step_id_of(input: &StepInput) -> String {
    input
        .unit
        .phase_id()
        .map(str::to_string)
        .unwrap_or_else(|| format!("unit-{}", input.unit.ord))
}

fn envelope(run_id: &str, ord: u32, attempt: u32, by: &str, re: Option<String>) -> Envelope {
    Envelope {
        run_id: run_id.to_string(),
        ord: Some(ord),
        attempt: Some(attempt),
        by: by.to_string(),
        at: crate::interaction::now_millis(),
        re,
    }
}

/// An empty ledger for an attempt no final pass ran on.
fn empty_ledger(final_pass: FinalPass) -> TeamLedger {
    TeamLedger::new(final_pass, Vec::new(), Vec::new(), Default::default())
}

fn empty_transcript() -> Transcript {
    Transcript {
        from_event_id: 0,
        to_event_id: 0,
        count: 0,
        truncated: false,
        events: Vec::new(),
    }
}

/// The local snapshot of a team unit that runs un-teamed (§4.1 "One db", §4.8 rows 1/5/6):
/// `transport: none`, the reason, an empty ledger and an empty transcript. Never published.
pub fn local_snapshot(stamped: &UnitTeamSnapshot, reason: Option<String>) -> UnitTeamSnapshot {
    UnitTeamSnapshot {
        transport: Transport::None,
        reason: reason.or_else(|| stamped.reason.clone()),
        ledger_source: stamped.ledger_source.filter(|s| *s == LedgerSource::NoBus),
        stream_floor: None,
        claimed_event_id: None,
        ledger_ref: None,
        ledger: Some(empty_ledger(FinalPass::Skipped)),
        transcript: Some(empty_transcript()),
    }
}

/// Publish a required R fact within the bound: `Ok(Some(id))` on the bus, `Ok(None)` superseded
/// (a tombstone covers it), `Err(why)` past the bound or unkeyable/unspoolable.
fn publish_bounded(runner: &TeamRunner, ev: &TeamEvent) -> Result<Option<i64>, String> {
    let mut last = String::new();
    for (i, delay) in std::iter::once(Duration::ZERO)
        .chain(runner.schedule.iter().copied())
        .enumerate()
    {
        if i > 0 {
            std::thread::sleep(delay);
        }
        match runner.bus.publish(ev) {
            Ok(PublishOutcome::Published(id)) => return Ok(Some(id)),
            Ok(PublishOutcome::Superseded) => return Ok(None),
            Ok(PublishOutcome::Spooled(why)) => last = why,
            Err(e) => return Err(format!("{e:#}")),
        }
    }
    let bound: Duration = runner.schedule.iter().sum();
    Err(format!(
        "{} could not be published: the bus refused it for {} attempts over {bound:.0?} (last: \
         {last})",
        ev.event_type(),
        runner.schedule.len() + 1
    ))
}

/// Claim the attempt before its turn starts (§4.3: `step.claimed` has a lower `event_id` than any
/// row of the attempt's turn). `Err` = the step must FAIL before its turn: `step.claimed` failed
/// and its tombstone could not be written, so the spooled fact could still land for an attempt
/// that ran un-teamed.
pub fn claim(runner: Option<&TeamRunner>, input: &StepInput) -> Result<Attempt, String> {
    let Some(stamped) = input.unit.team.as_ref() else {
        return Ok(Attempt::NotTeam);
    };
    if stamped.transport == Transport::None {
        return Ok(Attempt::Local(Box::new(local_snapshot(stamped, None))));
    }
    // Stamped `bus` at dispatch, but this worker has no runner: the bus was present at launch and
    // is absent now. Nothing of the attempt was generated, so nothing needs a tombstone.
    let Some(runner) = runner else {
        return Ok(Attempt::Local(Box::new(local_snapshot(
            stamped,
            Some(
                "step.claimed not published: this worker has no team transport (the bus was \
                 present at dispatch and is absent now)"
                    .into(),
            ),
        ))));
    };
    let Some(stream_floor) = stamped.stream_floor else {
        return Ok(Attempt::Local(Box::new(local_snapshot(
            stamped,
            Some("step.claimed not published: the dispatch stamped no stream floor".into()),
        ))));
    };
    let (run_id, ord, attempt) = (input.run_id.clone(), input.unit.ord, input.attempt);
    let by = seat_of(input);
    let step_id = step_id_of(input);
    let baseline = input.unit.worktree_baseline.as_ref();
    let reviewing = input.unit.member_step.as_ref().and_then(|m| m.reviewing);
    let criterion = input
        .unit
        .validator
        .as_ref()
        .map(|v| v.criterion.clone())
        .unwrap_or_else(|| input.unit.description.clone());
    let ev = TeamEvent {
        env: envelope(&run_id, ord, attempt, &by, None),
        body: TeamBody::StepClaimed(StepClaimed {
            step_id: step_id.clone(),
            role: input.unit.role.token().to_string(),
            // A unit carries no phase kind; what it IS for the team is an agent turn or the
            // engine's own tool command.
            kind: if input.unit.tool_cmd.is_some() {
                "tool".into()
            } else {
                "agent".into()
            },
            phase: input
                .unit
                .phase_id()
                .map(str::to_string)
                .unwrap_or_else(|| step_id.clone()),
            criterion: cap_utf8(
                input
                    .unit
                    .validator
                    .as_ref()
                    .map(|v| v.criterion.as_str())
                    .unwrap_or(""),
                2 * 1024,
            ),
            baseline_tree: baseline.map(|b| b.tree.clone()),
            repo: input.workdir.as_ref().map(|wd| tev::RepoRef {
                workdir: wd.to_string_lossy().into_owned(),
                git_dir: baseline.and_then(|b| b.git_dir.clone()).unwrap_or_default(),
            }),
            code_graph_db: input
                .governance
                .as_ref()
                .and_then(|g| g.code_graph_db.clone()),
        }),
    };
    let why = match publish_bounded(runner, &ev) {
        Ok(Some(id)) => {
            return Ok(Attempt::Claimed(Box::new(Claimed {
                runner: runner.clone(),
                run_id,
                ord,
                attempt,
                by,
                step_id,
                stream_floor,
                claimed_id: id,
                reviewing,
                criterion,
            })))
        }
        Ok(None) => "step.claimed superseded: the run or attempt moved past it".to_string(),
        Err(why) => why,
    };
    // Tombstone the attempt's runner lane FIRST (§4.1): its spooled `step.claimed`, and every R
    // line of the attempt from it on, is never published.
    runner
        .bus
        .supersede_run(
            &run_id,
            Some(tev::Owner::Runner),
            Some((ord, attempt)),
            tev::STEP_CLAIMED,
            &why,
        )
        .map_err(|e| {
            format!(
                "team transport: step.claimed failed ({why}) and the attempt's tombstone could \
                 not be written ({e}); the step does not start (fail closed)"
            )
        })?;
    Ok(Attempt::Local(Box::new(local_snapshot(
        stamped,
        Some(format!("un-teamed attempt: {why}")),
    ))))
}

/// The key of a raised finding: `(ord, attempt, raise_seq)` — each raise is one item.
type RaiseKey = (Option<u32>, Option<u32>, u32);

/// What the step-boundary injector did.
#[derive(Debug, Clone, Default)]
pub struct Boundary {
    /// The `[team advice]` prior-context block, when there was anything to render.
    pub block: Option<PriorUnitOutput>,
    /// `(finding_id, raise_seq)` of every finding the block rendered (one `advice.delivered` row
    /// was published for each).
    pub rendered: Vec<(String, u32)>,
    /// Why the stream could not be read (then nothing was rendered, and the gate treats those
    /// findings as undelivered, which is fail-closed at the gate: DES-001 §6.3).
    pub unread: Option<String>,
}

/// The step-boundary injector (§8.9): one bounded read of the run's stream from `path.started`,
/// rendered into one `[team advice]` prior-context block:
///
/// - every raised finding whose latest raise for its unit (`(ord, finding_id)`, the redriven
///   attempt's carried raise over the dead attempt's) has no `advice.delivered{outcome:
///   "injected"}` on ANY channel — HIGH and MEDIUM, within the 8 KB cap; a finding raised by an
///   earlier attempt of this unit is labelled `carried_from_attempt:<n>`. One
///   `advice.delivered{channel:"boundary", outcome:"injected"}` is published per rendered finding;
///   one that did not fit gets no row and is picked up at the next boundary;
/// - every `help.answered`, `council.ruled` and `change.requested` of the run that no LATER step of
///   this seat was already shown (no `step.claimed` by the same seat after the row, other than
///   this attempt's own): stateless, and a row that lands after a boundary read is shown at the
///   next one.
pub fn boundary(claimed: &Claimed) -> Boundary {
    let stream = match claimed
        .runner
        .read_run(&claimed.run_id, claimed.stream_floor.saturating_sub(1))
    {
        Ok(s) => s,
        Err(e) => {
            return Boundary {
                block: Some(PriorUnitOutput {
                    label: ADVICE_LABEL.to_string(),
                    output: format!(
                        "[wicked-core · team advice unavailable] The team's stream could not be \
                         read ({e:#}); undelivered findings reach the gate instead."
                    ),
                }),
                rendered: Vec::new(),
                unread: Some(format!("{e:#}")),
            }
        }
    };
    let mut injected: BTreeSet<(RaiseKey, String)> = BTreeSet::new();
    for row in &stream.rows {
        if let TeamBody::AdviceDelivered(b) = &row.event.body {
            if b.outcome == DeliveryOutcome::Injected {
                let env = &row.event.env;
                injected.insert(((env.ord, env.attempt, b.raise_seq), b.finding_id.clone()));
            }
        }
    }
    // The latest raise per (ord, finding_id): the attempt a carried finding now lives on.
    let mut latest: Vec<(RaiseKey, Finding)> = Vec::new();
    for row in &stream.rows {
        let TeamBody::FindingRaised(b) = &row.event.body else {
            continue;
        };
        let env = &row.event.env;
        let key = (env.ord, env.attempt, b.raise_seq);
        let mut f = finding_of(env, b);
        if f.carried_from_attempt.is_none()
            && env.ord == Some(claimed.ord)
            && env.attempt.is_some_and(|a| a < claimed.attempt)
        {
            f.carried_from_attempt = env.attempt;
        }
        match latest
            .iter_mut()
            .find(|(k, lf)| k.0 == key.0 && lf.finding_id == f.finding_id)
        {
            Some(slot) if slot.0 .1 < key.1 || (slot.0 .1 == key.1 && slot.0 .2 < key.2) => {
                *slot = (key, f)
            }
            Some(_) => {}
            None => latest.push((key, f)),
        }
    }
    let pending: Vec<(RaiseKey, Finding)> = latest
        .into_iter()
        .filter(|(k, f)| !injected.contains(&(*k, f.finding_id.clone())))
        .collect();
    let answers = team_answers(&stream.rows, claimed);
    if pending.is_empty() && answers.is_empty() {
        return Boundary::default();
    }
    let advice: Vec<super::Advice> = pending
        .iter()
        .map(|(_, f)| super::Advice { finding: f.clone() })
        .collect();
    let (mut text, sent, _rest) = if advice.is_empty() {
        (String::new(), Vec::new(), Vec::new())
    } else {
        super::advice_block(advice)
    };
    if !answers.is_empty() {
        let room = super::ADVICE_TEXT_CAP.saturating_sub(text.len());
        text.push_str(&cap_utf8(&answers, room));
    }
    let delivery_id = tev::delivery_id_boundary(&claimed.step_id, claimed.attempt);
    let mut rendered = Vec::new();
    for a in &sent {
        let Some(((ord, attempt, raise_seq), _)) = pending
            .iter()
            .find(|(_, f)| f.finding_id == a.finding.finding_id)
        else {
            continue;
        };
        let (Some(ord), Some(attempt)) = (*ord, *attempt) else {
            continue;
        };
        let ev = TeamEvent {
            env: envelope(
                &claimed.run_id,
                ord,
                attempt,
                "engine",
                Some(format!("finding.raised#{raise_seq}")),
            ),
            body: TeamBody::AdviceDelivered(AdviceDelivered {
                raise_seq: *raise_seq,
                finding_id: a.finding.finding_id.clone(),
                delivery_id: delivery_id.clone(),
                steer_id: None,
                channel: Channel::Boundary,
                outcome: DeliveryOutcome::Injected,
                detail: None,
            }),
        };
        // Spooled or superseded: the line waits in the outbox (or is dead) — the block was still
        // rendered, so the worker saw it; a later boundary may render it again until the row
        // lands (duplicate advice, never lost advice).
        if let Err(e) = claimed.runner.bus.publish(&ev) {
            eprintln!(
                "wicked-core: advice.delivered for {} not written ({e:#}); the next boundary \
                 renders it again",
                a.finding.finding_id
            );
        }
        rendered.push((a.finding.finding_id.clone(), *raise_seq));
    }
    Boundary {
        block: Some(PriorUnitOutput {
            label: ADVICE_LABEL.to_string(),
            output: text,
        }),
        rendered,
        unread: None,
    }
}

/// The team's answers the PA has not been shown yet (§8.9): `help.answered`, `council.ruled` and
/// `change.requested` rows of the run with no later `step.claimed` by this attempt's seat (other
/// than its own claim) — a claim after the row means that step's boundary read it.
fn team_answers(rows: &[TeamRow], claimed: &Claimed) -> String {
    let later_claim = |event_id: i64| {
        rows.iter().any(|r| {
            r.event_id > event_id
                && r.event_id != claimed.claimed_id
                && r.event.env.by == claimed.by
                && matches!(r.event.body, TeamBody::StepClaimed(_))
        })
    };
    let question = |help_id: &str| {
        rows.iter().find_map(|r| match &r.event.body {
            TeamBody::HelpRequested(h) if h.help_id == help_id => Some(h.question.clone()),
            _ => None,
        })
    };
    let mut out = String::new();
    for r in rows {
        if later_claim(r.event_id) {
            continue;
        }
        match &r.event.body {
            TeamBody::HelpAnswered(b) => out.push_str(&format!(
                "- help {} — you asked: {}\n  {} answers: {}{}\n",
                b.help_id,
                cap_utf8(&question(&b.help_id).unwrap_or_default(), 512),
                r.event.env.by,
                cap_utf8(&b.answer, 2 * 1024),
                if b.evidence.is_empty() {
                    String::new()
                } else {
                    format!(" (evidence: {})", b.evidence.join(", "))
                }
            )),
            TeamBody::CouncilRuled(b) => out.push_str(&format!(
                "- council ruling on {} (unit {}, attempt {}): {}{}{}\n",
                b.subject,
                r.event.env.ord.unwrap_or_default(),
                r.event.env.attempt.unwrap_or_default(),
                token(&b.verdict).to_ascii_uppercase(),
                b.reason
                    .map(|x| format!(" ({})", token(&x)))
                    .unwrap_or_default(),
                if b.dissent.is_empty() {
                    String::new()
                } else {
                    format!(" — dissent: {}", cap_utf8(&b.dissent.join(" | "), 512))
                }
            )),
            TeamBody::ChangeRequested(b) => out.push_str(&format!(
                "- change requested {} by {}: steps [{}] — {}. Answer `PLAN {}: ACCEPT` and on the \
                 next line the steps you accept as `PLAN+ {{\"steps\":[{{\"catalog\":\"…\"}}]}}`, \
                 or `PLAN {}: DECLINE — <why>`.\n",
                b.change_id,
                r.event.env.by,
                b.steps
                    .iter()
                    .map(|s| format!("{}:{}", s.catalog, s.id))
                    .collect::<Vec<_>>()
                    .join(", "),
                cap_utf8(&b.reason, 512),
                b.change_id,
                b.change_id
            )),
            _ => {}
        }
    }
    if out.is_empty() {
        return out;
    }
    format!("\n[team answers · ADVISORY: the team's replies since your last step]\n{out}")
}

fn completion(status: StepStatus) -> StepCompletion {
    match status {
        StepStatus::Ok => StepCompletion::Ok,
        StepStatus::Failed => StepCompletion::Failed,
        StepStatus::Cancelled => StepCompletion::Cancelled,
        StepStatus::ElicitationFailed => StepCompletion::ElicitationFailed,
        StepStatus::TimedOut => StepCompletion::TimedOut,
    }
}

/// The attempt's rows from its `step.claimed` on, as a capped transcript.
pub(crate) fn transcript_of(rows: &[TeamRow], cap: usize) -> Transcript {
    let mut events = Vec::new();
    let mut size = 0usize;
    let mut truncated = false;
    for r in rows {
        let payload = r.event.to_payload().unwrap_or(Value::Null);
        let len = payload.to_string().len() + r.event.event_type().len() + 32;
        if size + len > cap {
            truncated = true;
            break;
        }
        size += len;
        events.push(TranscriptRow {
            event_id: r.event_id,
            event_type: r.event.event_type().to_string(),
            payload,
        });
    }
    Transcript {
        from_event_id: rows.first().map_or(0, |r| r.event_id),
        to_event_id: rows.last().map_or(0, |r| r.event_id),
        count: rows.len() as u32,
        truncated,
        events,
    }
}

/// The attempt's own rows: `(ord, attempt)` of the attempt, from its `step.claimed` on.
fn attempt_rows(claimed: &Claimed) -> anyhow::Result<(Vec<TeamRow>, usize)> {
    let s = claimed
        .runner
        .read_run(&claimed.run_id, claimed.claimed_id.saturating_sub(1))?;
    let rows = s
        .rows
        .into_iter()
        .filter(|r| {
            r.event.env.ord == Some(claimed.ord) && r.event.env.attempt == Some(claimed.attempt)
        })
        .collect();
    Ok((rows, s.malformed))
}

/// After the turn: publish `step.completed`, then wait (bounded) for S's `ledger.folded` of this
/// attempt (§8.11). On timeout, synthesize DES-001 §4.7's fail-closed ledger from the attempt's
/// own rows WITHOUT publishing it. The result is the attempt's snapshot for `UnitEvidence.team`.
pub fn complete(claimed: &Claimed, output: &StepOutput) -> UnitTeamSnapshot {
    complete_at(
        claimed,
        output,
        Instant::now() + claimed.runner.final_pass_budget,
    )
}

fn snapshot(
    claimed: &Claimed,
    source: LedgerSource,
    ledger_ref: Option<String>,
    ledger: TeamLedger,
    transcript: Transcript,
) -> UnitTeamSnapshot {
    UnitTeamSnapshot {
        transport: Transport::Bus,
        reason: None,
        ledger_source: Some(source),
        stream_floor: Some(claimed.stream_floor),
        claimed_event_id: Some(claimed.claimed_id),
        ledger_ref,
        ledger: Some(ledger),
        transcript: Some(transcript),
    }
}

/// The PA's lines at the end of its turn (§8.8, §8.9), published BEFORE `step.completed` on the
/// same runner lane, so the supervisor's final pass reads them in order: one `advice.answered` per
/// `ADVICE` line naming a raised finding, one `help.requested` per `HELP:` line, and — on the PA's
/// review of a member's step — one `step.reviewed` from its `STEP` line. A failed turn has no final
/// answer to read. A line that cannot be published is logged: a missing answer leaves its finding
/// unanswered (the council path), a missing question is simply not answered, and a missing review
/// is read from the same output by the engine.
fn publish_turn_lines(claimed: &Claimed, output: &StepOutput) {
    if output.status != StepStatus::Ok {
        return;
    }
    let publish = |ev: TeamEvent| {
        if let Err(e) = claimed.runner.bus.publish(&ev) {
            eprintln!(
                "wicked-core: {} of {}:{}:{} not written ({e:#})",
                ev.event_type(),
                claimed.run_id,
                claimed.ord,
                claimed.attempt
            );
        }
    };
    let answers = super::parse_advice_lines(&output.output);
    if !answers.is_empty() {
        match claimed
            .runner
            .read_run(&claimed.run_id, claimed.stream_floor.saturating_sub(1))
        {
            Ok(stream) => {
                for (finding_id, resp) in &answers {
                    // This attempt's raise of it first, else the latest raise of this unit.
                    let raise = stream
                        .rows
                        .iter()
                        .filter_map(|r| match &r.event.body {
                            TeamBody::FindingRaised(b)
                                if b.finding_id == *finding_id
                                    && r.event.env.ord == Some(claimed.ord) =>
                            {
                                Some((r.event.env.attempt.unwrap_or_default(), b.raise_seq))
                            }
                            _ => None,
                        })
                        .max_by_key(|(a, seq)| ((*a == claimed.attempt), *a, *seq));
                    let Some((attempt, raise_seq)) = raise else {
                        continue;
                    };
                    publish(TeamEvent {
                        env: envelope(
                            &claimed.run_id,
                            claimed.ord,
                            attempt,
                            &claimed.by,
                            Some(format!("finding.raised#{raise_seq}")),
                        ),
                        body: TeamBody::AdviceAnswered(tev::AdviceAnswered {
                            raise_seq,
                            answered_in: tev::answered_in(&claimed.step_id, claimed.attempt),
                            finding_id: finding_id.clone(),
                            disposition: match resp.disposition {
                                super::Disposition::Accepted => tev::AdviceDisposition::Accepted,
                                super::Disposition::Declined => tev::AdviceDisposition::Declined,
                            },
                            reason: resp.reason.clone(),
                        }),
                    });
                }
            }
            Err(e) => eprintln!(
                "wicked-core: unit {} attempt {}: ADVICE lines not recorded, the stream could not \
                 be read ({e:#}); the findings stay unanswered (the council path)",
                claimed.ord, claimed.attempt
            ),
        }
    }
    for (i, question) in super::parse_help_lines(&output.output)
        .into_iter()
        .enumerate()
    {
        let help_seq = i as u32 + 1;
        publish(TeamEvent {
            env: envelope(
                &claimed.run_id,
                claimed.ord,
                claimed.attempt,
                &claimed.by,
                None,
            ),
            body: TeamBody::HelpRequested(tev::HelpRequested {
                help_id: tev::mint_help_id(
                    &claimed.run_id,
                    claimed.ord,
                    claimed.attempt,
                    &claimed.by,
                    help_seq,
                ),
                help_seq,
                question,
                context: cap_utf8(
                    &format!("step {}: {}", claimed.step_id, claimed.criterion),
                    super::HELP_CAP,
                ),
            }),
        });
    }
    if let Some(reviewed_attempt) = claimed.reviewing {
        if let Some(line) = super::parse_step_lines(&output.output)
            .into_iter()
            .find(|l| l.step_id == claimed.step_id)
        {
            publish(TeamEvent {
                env: envelope(
                    &claimed.run_id,
                    claimed.ord,
                    claimed.attempt,
                    &claimed.by,
                    Some(format!(
                        "step.completed#{}:{reviewed_attempt}",
                        claimed.step_id
                    )),
                ),
                body: TeamBody::StepReviewed(tev::StepReviewed {
                    step_id: line.step_id,
                    verdict: line.verdict,
                    to: line.to,
                    reason: line.reason,
                    reviewed_attempt: Some(reviewed_attempt),
                }),
            });
        }
    }
}

fn complete_at(claimed: &Claimed, output: &StepOutput, deadline: Instant) -> UnitTeamSnapshot {
    publish_turn_lines(claimed, output);
    let completed = TeamEvent {
        env: envelope(
            &claimed.run_id,
            claimed.ord,
            claimed.attempt,
            &claimed.by,
            None,
        ),
        body: TeamBody::StepCompleted(StepCompleted {
            step_id: claimed.step_id.clone(),
            status: completion(output.status),
            tree: None,
            output_bytes: output.output.len() as u64,
            output_ref: format!(
                "unit:{}:{}:{}",
                claimed.run_id, claimed.ord, claimed.attempt
            ),
        }),
    };
    let mut completed_id: Option<i64> = None;
    let publish_completed = |completed_id: &mut Option<i64>| {
        if completed_id.is_none() {
            if let Ok(PublishOutcome::Published(id)) = claimed.runner.bus.publish(&completed) {
                *completed_id = Some(id);
            }
        }
    };
    publish_completed(&mut completed_id);
    // A failed step skips the final pass (§8.11): it fails on its own account and never reaches
    // the gate, so there is no gate for a ledger to feed. `step.completed` still tells S.
    if output.status != StepStatus::Ok {
        return snapshot(
            claimed,
            LedgerSource::Synthesized,
            None,
            empty_ledger(FinalPass::Skipped),
            empty_transcript(),
        );
    }
    let db = BusDb::shared(&claimed.runner.bus_db);
    let mut floor = claimed.claimed_id;
    loop {
        publish_completed(&mut completed_id);
        if let (Some(id), Ok(db)) = (completed_id, db.as_ref()) {
            floor = floor.max(id);
            if let Ok(rows) = db.poll(tev::LEDGER_FOLDED, floor, 50) {
                for ev in rows {
                    // Past every row inspected: `poll` is strictly-after, so a cursor that stays
                    // put re-reads the same batch forever behind other attempts' folds.
                    floor = floor.max(ev.event_id);
                    let Ok(te) = TeamEvent::from_payload(&ev.event_type, &ev.payload) else {
                        continue;
                    };
                    if te.env.run_id != claimed.run_id
                        || te.env.ord != Some(claimed.ord)
                        || te.env.attempt != Some(claimed.attempt)
                    {
                        continue;
                    }
                    if let TeamBody::LedgerFolded(f) = te.body {
                        return snapshot(
                            claimed,
                            LedgerSource::Folded,
                            Some(format!("ledger.folded#{}:{}", claimed.ord, claimed.attempt)),
                            f.ledger,
                            f.transcript,
                        );
                    }
                }
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(claimed.runner.gate_poll.min(deadline - Instant::now()));
    }
    // Timed out. A `step.completed` still spooled must never land after the gate decided.
    if completed_id.is_none() {
        if let Ok(key) = completed.key() {
            if let Err(e) = claimed
                .runner
                .bus
                .supersede_fact(&key, "step.completed past the final-pass budget")
            {
                eprintln!("wicked-core: step.completed tombstone not written ({e})");
            }
        }
    }
    // A fact of this run still waiting in the outbox (S's `finding.raised` among them) is not in
    // the rows the synthesis folds: the record is incomplete, so it is `stream_gap` (pauses) —
    // never a timed-out ledger missing a finding the bus has not seen.
    let spooled = claimed.runner.bus.run_has_pending(&claimed.run_id);
    let (ledger, transcript) = match attempt_rows(claimed) {
        Ok((rows, malformed)) => {
            let folded = tev::fold(&rows);
            let ledger = if spooled || malformed > 0 || folded.final_pass == FinalPass::StreamGap {
                // An incomplete record stays `stream_gap`: it pauses, never auto-approves.
                let mut l = folded;
                l.final_pass = FinalPass::StreamGap;
                l.refresh_pause();
                l
            } else {
                tev::synthesize_timeout(folded)
            };
            (ledger, transcript_of(&rows, TRANSCRIPT_CAP))
        }
        Err(e) => {
            eprintln!(
                "wicked-core: unit {} attempt {}: the team stream could not be read at the \
                 final-pass timeout ({e:#}); the ledger is stream_gap (pauses)",
                claimed.ord, claimed.attempt
            );
            (empty_ledger(FinalPass::StreamGap), empty_transcript())
        }
    };
    snapshot(claimed, LedgerSource::Synthesized, None, ledger, transcript)
}

/// The seats that authored or corroborated any finding of the ledger (any status), in first-seen
/// order, deduplicated (DES-001 §6.2 `ledger_authors`). Computed from the ledger, never supplied.
pub fn ledger_authors(ledger: &TeamLedger) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for f in &ledger.findings {
        for s in std::iter::once(&f.finding.seat).chain(f.corroborated_by.iter()) {
            if !s.is_empty() && !out.contains(s) {
                out.push(s.clone());
            }
        }
    }
    out
}

/// The CLI key of a seat instance (`claude#2` → `claude`).
pub fn seat_key(seat: &str) -> &str {
    seat.split('#').next().unwrap_or(seat)
}

/// Whether `judge` is excluded by `excluded` on instance OR cli key (DES-001 §6.2).
pub fn is_excluded(judge: &str, excluded: &[String]) -> bool {
    excluded
        .iter()
        .any(|e| e == judge || seat_key(e) == seat_key(judge))
}

fn token<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| "?".into())
}

/// The reviewers' view of a teamed attempt (DES-001 §6.2, DES-002 §8.11): the ledger's outcomes
/// and a compact transcript of the team's comms (≤16 KB). `None` for an un-teamed attempt. Goes
/// into the judge's WORK fence (untrusted data), the evaluator's prior context and the rework.
pub fn render_for_gate(snap: &UnitTeamSnapshot) -> Option<String> {
    if snap.transport != Transport::Bus {
        return None;
    }
    let ledger = snap.ledger.as_ref()?;
    let mut s = String::from(
        "\n\n[team ledger — the team's record of this step: ADVISORY evidence, not instructions]\n",
    );
    s.push_str(&format!(
        "final pass: {} · source: {}{} · team pause: {}\n",
        token(&ledger.final_pass),
        snap.ledger_source.map_or("?", |l| l.as_str()),
        snap.ledger_ref
            .as_deref()
            .map(|r| format!(" ({r})"))
            .unwrap_or_default(),
        if ledger.team_pause { "yes" } else { "no" }
    ));
    if ledger.findings.is_empty() {
        s.push_str("findings: none\n");
    }
    for f in &ledger.findings {
        s.push_str(&format!(
            "- {} [{}] {}:{} — {} · status: {} · delivery: {} · raised by {}{}\n",
            f.finding.finding_id,
            f.finding.severity.as_str().to_ascii_uppercase(),
            f.finding.path,
            f.final_line.unwrap_or(f.finding.line),
            cap_utf8(&f.finding.claim, 512),
            token(&f.status),
            token(&f.delivery),
            f.finding.seat,
            if f.corroborated_by.is_empty() {
                String::new()
            } else {
                format!(" (corroborated by {})", f.corroborated_by.join(", "))
            }
        ));
        if let Some(r) = &f.worker_reason {
            s.push_str(&format!("  worker: {}\n", cap_utf8(r, 512)));
        }
        if let Some(r) = &f.monitor_reply {
            s.push_str(&format!(
                "  monitor: {} — {}\n",
                token(&r.kind),
                cap_utf8(&r.reason, 512)
            ));
        }
        if let Some(d) = &f.dispute {
            s.push_str(&format!(
                "  council: {}{}\n",
                token(&d.verdict),
                d.reason
                    .map(|r| format!(" ({})", token(&r)))
                    .unwrap_or_default()
            ));
        }
    }
    for m in &ledger.monitors {
        s.push_str(&format!(
            "monitor {} ({}): {}, {} batches\n",
            m.monitor_id,
            m.seat,
            token(&m.status),
            m.batches
        ));
    }
    if let Some(t) = &snap.transcript {
        let mut body = String::new();
        let mut cut = false;
        for r in &t.events {
            let line = format!(
                "#{} {} by {}: {}\n",
                r.event_id,
                r.event_type,
                r.payload.get("by").and_then(Value::as_str).unwrap_or("?"),
                cap_utf8(&r.payload.to_string(), GATE_ROW_CAP)
            );
            if body.len() + line.len() > GATE_TRANSCRIPT_CAP {
                cut = true;
                break;
            }
            body.push_str(&line);
        }
        s.push_str(&format!(
            "[team transcript — {} rows, event ids {}..{}{}]\n{body}",
            t.count,
            t.from_event_id,
            t.to_event_id,
            if cut || t.truncated { ", capped" } else { "" }
        ));
    }
    Some(s)
}

/// The actor's merge of the dispatch stamp and the worker's snapshot (T5): the worker may only
/// DOWNGRADE the transport (a failed `step.claimed`), never upgrade it; a snapshot stamped `bus`
/// whose worker returned nothing — or returned a teamed snapshot without a ledger — becomes a
/// `stream_gap` ledger, which pauses. An un-teamed stamp keeps the stamp's facts and gains the
/// local empty ledger.
pub fn merge_snapshot(
    stamped: Option<&UnitTeamSnapshot>,
    worker: Option<UnitTeamSnapshot>,
) -> Option<UnitTeamSnapshot> {
    let stamped = stamped?;
    if stamped.transport == Transport::None {
        return Some(local_snapshot(stamped, None));
    }
    match worker {
        Some(w) if w.transport == Transport::None => Some(UnitTeamSnapshot {
            ledger_source: None,
            ..w
        }),
        Some(w) if w.ledger.is_some() => Some(UnitTeamSnapshot {
            transport: Transport::Bus,
            stream_floor: stamped.stream_floor,
            ..w
        }),
        _ => Some(UnitTeamSnapshot {
            transport: Transport::Bus,
            reason: Some(
                "the worker returned no team ledger for a teamed attempt (stream_gap: pauses)"
                    .into(),
            ),
            ledger_source: Some(LedgerSource::Synthesized),
            stream_floor: stamped.stream_floor,
            claimed_event_id: None,
            ledger_ref: None,
            ledger: Some(empty_ledger(FinalPass::StreamGap)),
            transcript: Some(empty_transcript()),
        }),
    }
}

#[cfg(test)]
#[path = "runner_tests.rs"]
mod tests;
