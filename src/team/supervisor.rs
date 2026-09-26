//! The team supervisor on the bus (DES-TEAMING-002 §4.2, §4.7, §8.8–§8.11, seam T6): the monitors'
//! host, re-homed from the engine's event fan-out onto a bus cursor.
//!
//! **One mechanism.** The supervisor reads `wicked.team.**` rows off the bus and publishes its own
//! facts (the S rows of DES-002 §7: `member.*`, `finding.*`, `help.answered`, `change.requested`,
//! `council.*`, `ledger.folded`) through [`TeamBus::publish`] on its own thread and connection. It
//! never reads a CoreEvent, never takes a direct command from a worker, and holds no buffer of rows
//! that arrived "too early": `step.claimed` is published before its turn starts (§4.3), so a
//! checkpoint for an attempt the supervisor has not seen claimed is not its to watch.
//!
//! **Restart = replay live runs, then tail (§4.7).** At spawn it snapshots the bus tail `T`, asks
//! the engine for its live teamed runs ([`LiveRuns`], P1's `Command::LiveTeamRuns`), folds each
//! run's rows from its `stream_floor` up to `T` into its state, and then polls live from `T`. An
//! attempt claimed before this process booted is dead (no attempt survives a restart): its
//! unaccepted findings are carried into the run's next attempt of that unit, marked
//! `carried_from_attempt`. A run whose `path.started` has aged out is a stream gap: an attempt whose
//! earlier rows are gone folds `stream_gap`, which pauses.
//!
//! **The attempt's final pass** runs when its `step.completed` row arrives (§8.11): one final batch
//! per member, re-confirmation against `T_final`, the hold round, one council per unresolved HIGH
//! (and per held member-step rejection), then `fold` over the attempt's rows, S's overlays, and
//! `ledger.folded` — or nothing past its deadline, where the spooled line is tombstoned.
//!
//! **Fail closed.** A council that cannot be convened, times out or is over the cap is a
//! `no_verdict` (it pauses); a member's silence in the hold round is HOLD; an S fact that did not
//! land makes the attempt's ledger `stream_gap`; a rejected member step whose member gave no
//! answer is recorded `held: null`, which the engine never reads as a counted step.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use super::events::{
    self as tev, AttachStatus, ChangeRequested, CouncilCalled, CouncilPosition, CouncilRuled,
    CouncilTrigger, Envelope, FindingRaised, FindingSettled, HelpAnswered, LedgerFolded,
    MemberJoined, MemberLeft, MemberRole, SettledStatus, StepCompletion, StepVerdict, TeamBody,
    TeamEvent, TeamRow, Transport,
};
use super::publish::{LiveTeamRun, PublishOutcome, TeamBus, TeamConfig};
use super::{
    cap_utf8, confirm, kind_may_change_tree, locate, parse_reply, AttachCtx, FinalPass, Finding,
    FindingBook, FindingStatus, LedgerMonitor, MonitorHost, MonitorScope, MonitorStatus,
    RawFinding, Rejected, ReplyKind, Repo, Severity, TeamLedger, TeamLimits, TeamPlan, UnitKey,
    Verdict,
};
use crate::bus::{BusDb, BusEvent};
use crate::decision::{DecisionRequest, DecisionVerdict};

/// The bus filter the supervisor's cursor reads (§4.2).
const TEAM_FILTER: &str = "wicked.team.**";

/// Rows per cursor read.
const READ_BATCH: usize = 500;

/// Councils per attempt (DES-001 §4.8 `MAX_DISPUTES`): past it a dispute gets `no_verdict{cap}`.
pub const MAX_DISPUTES: usize = 3;

/// Three is the ceiling (S4's bands): a monitor that flags everything is noise.
pub const MAX_MONITORS: u8 = 3;

/// How long the supervisor waits between cursor reads when nothing is due.
pub const SUPERVISOR_POLL: Duration = Duration::from_millis(250);

/// How often an unknown run's rows make the supervisor ask the engine whether it is live.
const ARM_RETRY: Duration = Duration::from_secs(5);

/// The share of the final-pass budget the supervisor keeps back to fold and publish (§8.11): the
/// work stops at `deadline - margin`, synthesizing what it could not finish.
fn margin_of(budget: Duration) -> Duration {
    (budget / 5).clamp(Duration::from_millis(50), Duration::from_secs(30))
}

fn now_ms() -> i64 {
    crate::interaction::now_millis()
}

// ── Seats: how many members, which ones ─────────────────────────────────────────────────────────

/// The monitor target (DES-002 §6 #3): `min(max(band monitors, the PA's ask), MAX_MONITORS)`. The
/// band's monitors are S4's (`plan_for(<band floor>)`); a band the stream does not state, or one
/// that does not parse, is the top band — a missing score never watches less (S4's "no graph scores
/// 100" rule).
pub fn monitor_target(band: Option<&str>, asked: u8) -> u8 {
    let floor = band
        .and_then(|b| b.split('-').next())
        .and_then(|lo| lo.trim().parse::<u8>().ok())
        .unwrap_or(100);
    crate::review_scale::plan_for(floor)
        .monitors
        .max(asked)
        .min(MAX_MONITORS)
}

// ── Councils (DES-001 §6.3, DES-002 §8.10) ──────────────────────────────────────────────────────

/// What one council call came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CouncilOutcome {
    /// `convene_decision` returned a verdict (a ruling, or none with its reason).
    Ruled(DecisionVerdict),
    /// No eligible non-party seat: nothing was convened (`seats_benched`).
    NoSeats(String),
    /// The call failed (`error`).
    Failed(String),
    /// No answer within the budget (`timeout`).
    TimedOut,
}

/// The council the supervisor convenes: production sends `Command::ConveneDecision` to the actor
/// ([`ActorCouncil`]); tests inject verdicts. `excluded` are the parties (the creator and every
/// author/corroborator): no seat whose instance or cli key is among them votes.
pub trait Council: Send + Sync {
    fn convene(
        &self,
        req: DecisionRequest,
        excluded: &[String],
        budget: Duration,
    ) -> CouncilOutcome;
}

/// The production council: the engine's single council entry point (`Core::convene_decision`,
/// ballots off-actor), reached over the actor's command channel with a bounded wait.
pub struct ActorCouncil {
    pub tx: Sender<crate::command::Command>,
}

impl Council for ActorCouncil {
    fn convene(
        &self,
        req: DecisionRequest,
        excluded: &[String],
        budget: Duration,
    ) -> CouncilOutcome {
        let clis: Vec<wicked_council::AgenticCli> = crate::registry_roster()
            .into_iter()
            .filter(|c| !super::runner::is_excluded(&c.key, excluded))
            .collect();
        if clis.is_empty() {
            return CouncilOutcome::NoSeats(format!(
                "no council seat is left after excluding the parties [{}]",
                excluded.join(", ")
            ));
        }
        let (reply, rx) = channel();
        if self
            .tx
            .send(crate::command::Command::ConveneDecision { req, clis, reply })
            .is_err()
        {
            return CouncilOutcome::Failed("the engine stopped".into());
        }
        match rx.recv_timeout(budget) {
            Ok(Ok(v)) => CouncilOutcome::Ruled(v),
            Ok(Err(e)) => CouncilOutcome::Failed(format!("{e:#}")),
            Err(RecvTimeoutError::Timeout) => CouncilOutcome::TimedOut,
            Err(RecvTimeoutError::Disconnected) => {
                CouncilOutcome::Failed("the engine dropped the council reply".into())
            }
        }
    }
}

/// The live teamed runs (P1's replay set, `Core::live_team_runs`).
pub type LiveRuns = Arc<dyn Fn() -> anyhow::Result<Vec<LiveTeamRun>> + Send + Sync>;

// ── Configuration ────────────────────────────────────────────────────────────────────────────────

/// Everything the supervisor needs, injectable: tests shorten every bound.
#[derive(Debug, Clone)]
pub struct SupervisorConfig {
    pub bus_db: String,
    pub outbox: PathBuf,
    pub attempt_wait: Duration,
    pub limits: TeamLimits,
    /// The worker's gate wait (`TeamConfig::final_pass_budget`): S publishes an attempt's
    /// `ledger.folded` within it, measured from the attempt's `step.completed`, or not at all.
    pub final_pass_budget: Duration,
    /// The cursor's poll interval.
    pub poll: Duration,
    pub max_disputes: usize,
    /// This process's boot, epoch ms: an attempt claimed before it is dead (§4.7).
    pub boot_ms: i64,
    /// The bus tail `T` snapshotted on the spawning thread; `None` = snapshot at spawn.
    pub tail: Option<i64>,
    /// How long an S lane's spooled facts are retried before they stay for `replay_team_outbox`.
    pub publish_bound: Duration,
}

impl SupervisorConfig {
    /// The supervisor for the engine's team config, or `None` with no bus or no outbox (then no
    /// run is ever teamed, §4.8 row 6).
    pub fn from_team(cfg: &TeamConfig, boot_ms: i64) -> Option<Self> {
        Some(Self {
            bus_db: cfg.bus_db.clone()?,
            outbox: cfg.outbox.clone()?,
            attempt_wait: cfg.attempt_wait,
            limits: TeamLimits::from_env(),
            final_pass_budget: cfg.final_pass_budget,
            poll: SUPERVISOR_POLL,
            max_disputes: MAX_DISPUTES,
            boot_ms,
            tail: None,
            publish_bound: cfg.bound(),
        })
    }

    fn bus(&self) -> TeamBus {
        TeamBus::new(self.bus_db.clone(), self.outbox.clone(), self.attempt_wait)
    }
}

// ── Publishing S facts ───────────────────────────────────────────────────────────────────────────

/// The supervisor's publish side: the one wrapper, plus the record of whether every fact of an
/// attempt landed (an unpublished S fact makes that attempt's ledger `stream_gap`).
#[derive(Clone)]
pub(crate) struct Publisher {
    bus: TeamBus,
    /// Runs whose S lane holds a spooled fact, and when the first one spooled: the cursor thread
    /// drains those lanes until [`SupervisorConfig::publish_bound`] runs out (§4.1: bounded;
    /// past it the lines stay for `replay_team_outbox`).
    spooled: Arc<Mutex<HashMap<String, Instant>>>,
    /// Spooled `ledger.folded` facts and their deadlines (epoch ms): past it, one still waiting is
    /// tombstoned, so a late drain never publishes a ledger the gate did not use (§4.1, §8.11).
    folds: Arc<Mutex<Vec<(String, i64)>>>,
}

impl Publisher {
    fn new(bus: TeamBus) -> Self {
        Self {
            bus,
            spooled: Arc::default(),
            folds: Arc::default(),
        }
    }

    /// Tombstone every spooled fold whose deadline has passed and forget the ones that landed.
    fn expire_folds(&self) {
        let now = now_ms();
        let mut folds = self.folds.lock().unwrap_or_else(|p| p.into_inner());
        folds.retain(|(key, deadline)| {
            if !self.bus.is_pending(key) {
                return false;
            }
            if now < *deadline {
                return true;
            }
            if let Err(e) = self
                .bus
                .supersede_fact(key, "ledger.folded past its deadline")
            {
                eprintln!(
                    "wicked-core: team: a late fold's tombstone was not written ({e}); retrying"
                );
                return true;
            }
            false
        });
    }

    /// [`Self::publish`], and whether the fact is LOST (not on the bus and not in the outbox).
    fn publish_tracked(&self, ev: &TeamEvent) -> (Option<i64>, bool) {
        match self.publish(ev) {
            Some(id) => (Some(id), false),
            None => (None, !self.bus.is_pending(&ev.key().unwrap_or_default())),
        }
    }

    /// Publish `ev`; `Some(event_id)` only when it is on the bus. Spooled, superseded or
    /// unwritable facts are logged and reported `None`.
    fn publish(&self, ev: &TeamEvent) -> Option<i64> {
        match self.bus.publish(ev) {
            Ok(PublishOutcome::Published(id)) => Some(id),
            Ok(PublishOutcome::Spooled(why)) => {
                eprintln!(
                    "wicked-core: team supervisor: {} of {} spooled ({why})",
                    ev.event_type(),
                    ev.env.run_id
                );
                self.spooled
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .entry(ev.env.run_id.clone())
                    .or_insert_with(Instant::now);
                None
            }
            Ok(other) => {
                eprintln!(
                    "wicked-core: team supervisor: {} of {} not on the bus ({other:?})",
                    ev.event_type(),
                    ev.env.run_id
                );
                None
            }
            Err(e) => {
                eprintln!(
                    "wicked-core: team supervisor: {} of {} not written ({e:#})",
                    ev.event_type(),
                    ev.env.run_id
                );
                None
            }
        }
    }
}

fn env_of(key: &UnitKey, by: &str, re: Option<String>) -> Envelope {
    Envelope {
        run_id: key.0.clone(),
        ord: Some(key.1),
        attempt: Some(key.2),
        by: by.to_string(),
        at: now_ms(),
        re,
    }
}

// ── One attempt's members (DES-001 §4.1–§4.6) ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum SlotState {
    /// Admitted, session not started yet (members open lazily, with their first batch).
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
    /// Openings so far (`member.joined.open_seq`).
    open_seq: u32,
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

/// One live attempt's team state: its members, its findings and what it published.
pub struct UnitTeam {
    ctx: AttachCtx,
    monitors: Vec<MonitorSlot>,
    summoned: bool,
    /// A tree-changing checkpoint arrived before any member was summoned.
    pending: bool,
    last_seq: u64,
    titles: Vec<String>,
    book: FindingBook,
    pub_: Publisher,
    /// Some S fact of this attempt never reached the bus or the outbox (unwritable, or
    /// superseded): its ledger is `stream_gap`. A spooled fact is checked at the fold instead.
    lost: bool,
    change_seq: u32,
    /// The `event_id` of each raise this attempt published, by `raise_seq`.
    raised_ids: BTreeMap<u32, i64>,
}

impl UnitTeam {
    fn new(ctx: AttachCtx, pub_: Publisher) -> Self {
        Self {
            ctx,
            monitors: Vec::new(),
            summoned: false,
            pending: false,
            last_seq: 0,
            titles: Vec::new(),
            book: FindingBook::default(),
            pub_,
            lost: false,
            change_seq: 0,
            raised_ids: BTreeMap::new(),
        }
    }

    fn key(&self) -> UnitKey {
        self.ctx.key()
    }

    fn publish(&mut self, ev: &TeamEvent) -> Option<i64> {
        let (id, lost) = self.pub_.publish_tracked(ev);
        self.lost |= lost;
        id
    }

    fn pool_key(&self, monitor_id: &str) -> String {
        format!(
            "team:{}:{}:{}:{monitor_id}",
            self.ctx.run_id, self.ctx.ord, self.ctx.attempt
        )
    }

    fn joined(&mut self, i: Option<usize>, id: &str, seat: &str, error: Option<String>) {
        let open_seq = match i {
            Some(i) => {
                self.monitors[i].open_seq += 1;
                self.monitors[i].open_seq
            }
            None => 1,
        };
        let key = self.key();
        let reason = format!("team plan monitors={}", self.ctx.plan.monitors);
        let ev = TeamEvent {
            env: env_of(&key, seat, None),
            body: TeamBody::MemberJoined(MemberJoined {
                member_id: id.to_string(),
                open_seq,
                seat: seat.to_string(),
                role: MemberRole::Monitor,
                status: if error.is_some() {
                    AttachStatus::Failed
                } else {
                    AttachStatus::Attached
                },
                reason,
                error,
            }),
        };
        self.publish(&ev);
    }

    /// Summon the plan's members (DES-001 §4.1): the first `plan.monitors` candidates that are
    /// NOT the creator's instance and ARE admitted. A refused candidate is disclosed as
    /// `member.joined{status:"failed"}` and starts no process. `plan.monitors == 0` summons
    /// nothing and publishes nothing.
    fn summon(&mut self, host: &dyn MonitorHost) {
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
            let candidates = self.ctx.plan.candidates.clone();
            for (i, seat) in candidates.iter().take(want).enumerate() {
                self.joined(
                    None,
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
                    "'{seat}' is the creator's own seat instance — a member must be distinct"
                ))
            } else {
                host.admitted(seat).err()
            };
            if let Some(why) = refusal {
                self.joined(None, &id, seat, Some(why));
                continue;
            }
            admitted += 1;
            let pool_key = self.pool_key(&id);
            self.monitors.push(MonitorSlot {
                id,
                seat: seat.clone(),
                pool_key,
                state: SlotState::Pending,
                open_seq: 0,
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
            "[wicked-core · team member · READ-ONLY reviewer]\n\
             You are a member of the team on a step another agent is working on right now. You \
             advise; you do not decide. The worker may refuse you with evidence, and the gate \
             decides.\n\
             Step criterion: {criterion}\nPhase: {phase}\n\
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
             If the plan needs more steps, you may ask for them with one line\n\
             CHANGE {{\"steps\":[{{\"catalog\":\"<catalog id>\",\"id\":\"<step id>\"}}],\
             \"reason\":\"<why>\"}}\n\
             then a final line DONE. Anything else is ignored.\n",
            criterion = self.ctx.criterion,
            phase = self.ctx.phase,
        )
    }

    fn job(&self, i: usize, final_tree: Option<String>, budget: Duration, cap: usize) -> BatchJob {
        let m = &self.monitors[i];
        BatchJob {
            key: self.key(),
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
        }
    }

    /// Raise one admitted finding: `finding.raised` with the attempt's next `raise_seq`.
    fn raise(&mut self, f: &Finding, seq: u32, corroborated_by: Vec<String>) {
        let key = self.key();
        let ev = TeamEvent {
            env: env_of(
                &key,
                &f.seat,
                (f.checkpoint_seq > 0).then(|| format!("checkpoint.reached#{}", f.checkpoint_seq)),
            ),
            body: TeamBody::FindingRaised(FindingRaised {
                raise_seq: seq,
                finding_id: f.finding_id.clone(),
                member_id: f.monitor_id.clone(),
                line_key: Some(super::line_key(&f.path, &f.evidence)),
                anchor: (!f.anchor.is_empty()).then(|| f.anchor.clone()),
                anchor_source: Some(if f.anchor.is_empty() {
                    tev::AnchorSource::None
                } else {
                    tev::AnchorSource::Hunk
                }),
                severity: f.severity,
                path: f.path.clone(),
                line: f.line,
                evidence: f.evidence.clone(),
                claim: f.claim.clone(),
                suggestion: f.suggestion.clone(),
                tree: f.tree.clone(),
                in_diff: f.in_diff,
                corroborated_by,
                carried_from_attempt: f.carried_from_attempt,
            }),
        };
        if let Some(id) = self.publish(&ev) {
            self.raised_ids.insert(seq, id);
        }
    }

    /// Carry a dead attempt's unaccepted finding into this attempt (§4.7 replay step 3): the same
    /// finding, re-raised under this attempt's `raise_seq`, marked with the attempt that first
    /// raised it. The final pass re-confirms it against this attempt's `T_final`.
    fn carry(&mut self, mut f: Finding, from_attempt: u32) {
        f.carried_from_attempt = Some(f.carried_from_attempt.unwrap_or(from_attempt));
        // The dead attempt's member id is not this attempt's: keep it distinct, so it is never
        // mistaken for (or deduplicated against) a member watching this attempt.
        f.monitor_id = format!("a{from_attempt}:{}", f.monitor_id);
        if let super::Admit::New(lf, seq) = self.book.admit(f) {
            let f = lf.finding.clone();
            self.raise(&f, seq, Vec::new());
        }
    }

    /// Fold one batch's outcome in: publish the opening, then dedup and raise the survivors.
    fn apply(&mut self, done: BatchDone) {
        let slot = done.slot;
        if slot >= self.monitors.len() {
            return;
        }
        self.monitors[slot].in_flight = false;
        if let Some(opened) = done.opened.clone() {
            let (id, seat) = (
                self.monitors[slot].id.clone(),
                self.monitors[slot].seat.clone(),
            );
            self.joined(Some(slot), &id, &seat, opened.err());
        }
        match done.outcome {
            BatchOutcome::OpenFailed(e) => {
                let m = &mut self.monitors[slot];
                m.state = SlotState::Failed;
                m.error = Some(e);
            }
            BatchOutcome::SnapshotFailed(e) => self.monitors[slot].error = Some(e),
            // The tree did not move: nothing opened, nothing spent.
            BatchOutcome::Skipped => {}
            // The host EVICTS the session on any failed turn, so the slot goes back to `Pending`:
            // the next batch reopens it (a new `member.joined`, the next `open_seq`).
            BatchOutcome::TurnFailed { error, timed_out } => {
                let m = &mut self.monitors[slot];
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
                changes,
            } => {
                let m = &mut self.monitors[slot];
                m.state = SlotState::Open;
                m.batches += 1;
                m.primed = true;
                m.last_tree = tree.clone();
                let (id, seat) = (m.id.clone(), m.seat.clone());
                self.book.rejected.add(rejected);
                for c in candidates {
                    let finding = Finding {
                        finding_id: String::new(),
                        monitor_id: id.clone(),
                        seat: seat.clone(),
                        severity: c.severity,
                        path: c.raw.path,
                        line: c.raw.line,
                        // Confirmed exact and within the cap (`run_job`): never truncated.
                        evidence: c.raw.evidence,
                        claim: cap_utf8(&c.raw.claim, super::CLAIM_CAP),
                        suggestion: c
                            .raw
                            .suggestion
                            .as_deref()
                            .map(|s| cap_utf8(s, super::CLAIM_CAP)),
                        tree: tree.clone(),
                        in_diff: c.in_diff,
                        checkpoint_seq: done.checkpoint_seq,
                        anchor: c.anchor,
                        carried_from_attempt: None,
                    };
                    if let super::Admit::New(lf, seq) = self.book.admit(finding) {
                        let f = lf.finding.clone();
                        self.raise(&f, seq, Vec::new());
                    }
                }
                for (steps, reason) in changes {
                    self.change_seq += 1;
                    let key = self.key();
                    let ev = TeamEvent {
                        env: env_of(&key, &seat, None),
                        body: TeamBody::ChangeRequested(ChangeRequested {
                            change_id: tev::mint_change_id(
                                &key.0,
                                key.1,
                                key.2,
                                &id,
                                self.change_seq,
                            ),
                            change_seq: self.change_seq,
                            steps,
                            reason,
                        }),
                    };
                    self.publish(&ev);
                }
            }
        }
    }

    /// The members as the ledger reports them (DES §7 `teamLedger.monitors[]`).
    fn ledger_monitors(&self) -> Vec<LedgerMonitor> {
        self.monitors
            .iter()
            .map(|m| LedgerMonitor {
                monitor_id: m.id.clone(),
                seat: m.seat.clone(),
                batches: m.batches,
                status: slot_status(m),
                error: m.error.clone(),
            })
            .collect()
    }
}

fn slot_status(m: &MonitorSlot) -> MonitorStatus {
    if m.state == SlotState::Failed {
        MonitorStatus::Failed
    } else if m.budget_exhausted {
        MonitorStatus::BudgetExhausted
    } else if m.timed_out {
        MonitorStatus::TimedOut
    } else {
        MonitorStatus::Completed
    }
}

// ── Batches (DES-001 §4.3–§4.6) ─────────────────────────────────────────────────────────────────

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
}

/// A confirmed, above-bar finding a batch found, before dedup.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    raw: RawFinding,
    severity: Severity,
    in_diff: bool,
    anchor: String,
}

#[derive(Debug, Clone, PartialEq)]
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
        candidates: Vec<Candidate>,
        rejected: Rejected,
        changes: Vec<(Vec<tev::PlanStep>, String)>,
    },
}

/// A finished batch, folded back in by the supervisor.
#[derive(Debug, Clone)]
pub struct BatchDone {
    pub key: UnitKey,
    slot: usize,
    checkpoint_seq: u64,
    /// `Some` when the batch opened the member's session (its result).
    opened: Option<Result<(), String>>,
    outcome: BatchOutcome,
}

const TITLES_PER_BATCH: usize = 20;

/// Run one batch: snapshot once and skip when the tree is unchanged; else open the session if
/// this is its first reviewed batch (members open lazily), prompt the member with the incremental
/// diff, and put every `FINDING` through parse → bar → confirm → anchor.
pub fn run_job(job: &BatchJob, host: &dyn MonitorHost) -> BatchDone {
    let done = |opened, outcome| BatchDone {
        key: job.key.clone(),
        slot: job.slot,
        checkpoint_seq: job.checkpoint_seq,
        opened,
        outcome,
    };
    let Some(repo) = job.repo.as_ref() else {
        return done(
            None,
            BatchOutcome::SnapshotFailed("no worktree".to_string()),
        );
    };
    let tree = match job.final_tree.clone() {
        Some(t) => t,
        None => match repo.snapshot() {
            Ok(t) => t,
            Err(e) => return done(None, BatchOutcome::SnapshotFailed(e.to_string())),
        },
    };
    // An `execute` that wrote nothing costs one snapshot — no session, no model turn.
    if tree == job.from_tree {
        return done(None, BatchOutcome::Skipped);
    }
    let mut opened = None;
    if job.needs_open {
        let r = host.open(&job.pool_key, &job.seat, &job.scope);
        opened = Some(r.clone());
        if let Err(e) = r {
            return done(opened, BatchOutcome::OpenFailed(e));
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
            prompt.push_str(&format!("- {}\n", cap_utf8(t, super::TITLE_CAP)));
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
            return done(opened, BatchOutcome::TurnFailed { error, timed_out });
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
        // unconfirmed — never confirmed exact and then truncated.
        if f.evidence.len() > super::EVIDENCE_CAP {
            rejected.unconfirmed += 1;
            eprintln!(
                "wicked-core: team: {}:{}:{} member {} finding at {}:{} rejected as unconfirmed: \
                 evidence over cap ({} B > {} B)",
                job.key.0,
                job.key.1,
                job.key.2,
                job.monitor_id,
                f.path,
                f.line,
                f.evidence.len(),
                super::EVIDENCE_CAP
            );
            continue;
        }
        let text = repo.file(&tree, &f.path);
        if !confirm(text.as_deref(), f.line, &f.evidence) {
            rejected.unconfirmed += 1;
            continue;
        }
        let in_diff = in_diff_paths.contains(&f.path);
        let anchor = super::anchor_of(text.as_deref(), f.line);
        candidates.push(Candidate {
            raw: f,
            severity,
            in_diff,
            anchor,
        });
    }
    done(
        opened,
        BatchOutcome::Reviewed {
            tree,
            candidates,
            rejected,
            changes: super::parse_change_lines(&reply),
        },
    )
}

// ── The run's stream, as the supervisor keeps it ────────────────────────────────────────────────

/// What the stream says about one attempt (from replay and from the live tail alike).
#[derive(Debug, Clone, Default)]
struct AttemptSeen {
    claimed_id: i64,
    claimed_at: i64,
    by: String,
    completed: bool,
    folded: bool,
    /// Claimed by THIS process (its runner is alive to wait for a fold).
    live: bool,
    raised: Vec<(Envelope, FindingRaised)>,
    accepted: BTreeSet<String>,
    closed: BTreeSet<String>,
}

impl AttemptSeen {
    /// The findings a dead attempt leaves unresolved: raised, and neither accepted by the worker
    /// nor withdrawn / superseded.
    fn unresolved(&self) -> Vec<(Envelope, FindingRaised)> {
        self.raised
            .iter()
            .filter(|(_, f)| !self.accepted.contains(&f.finding_id))
            .filter(|(_, f)| !self.closed.contains(&f.finding_id))
            .cloned()
            .collect()
    }
}

#[derive(Debug, Clone, Default)]
struct RunState {
    floor: i64,
    /// The PA seat instance (`path.started.cli`): never a member candidate.
    pa: String,
    roster: Vec<String>,
    band: Option<String>,
    asked: u8,
    /// `path.started` was missing at replay: rows before the gap are gone (§4.7).
    gap: bool,
    attempts: BTreeMap<(u32, u32), AttemptSeen>,
}

impl RunState {
    /// The attempt's member plan: the target from the band and the PA's ask, the candidates in
    /// roster order minus the PA (the PA is never a member; a creator among the candidates is
    /// refused and disclosed by [`UnitTeam::summon`]).
    fn plan(&self) -> TeamPlan {
        TeamPlan {
            monitors: monitor_target(self.band.as_deref(), self.asked),
            candidates: self
                .roster
                .iter()
                .filter(|s| **s != self.pa)
                .cloned()
                .collect(),
        }
    }
}

// ── The supervisor core (synchronous: the thread drives it, tests pump it) ──────────────────────

/// What the core asks its driver to run off the cursor thread.
pub enum Job {
    /// One member batch; its result comes back through [`SupervisorCore::apply_batch`].
    Batch(Box<BatchJob>),
    /// An attempt's final pass (publishes its own facts, then `ledger.folded`).
    FinalPass(Box<FinalPassJob>),
    /// A member's turn answering the PA's `HELP:` question.
    Help(HelpJob),
}

/// The supervisor's state over every armed run.
pub struct SupervisorCore {
    pub(crate) cfg: SupervisorConfig,
    runs: HashMap<String, RunState>,
    units: HashMap<UnitKey, Arc<Mutex<UnitTeam>>>,
    host: Arc<dyn MonitorHost>,
    council: Arc<dyn Council>,
    pub_: Publisher,
    /// Runs whose rows arrived while they were not armed, and when the engine was last asked.
    unknown: HashMap<String, Option<Instant>>,
    /// Runs whose `path.ended` was seen.
    ended: HashSet<String>,
    /// Help questions already taken (`help_id`).
    helped: HashSet<String>,
}

impl SupervisorCore {
    pub fn new(
        cfg: SupervisorConfig,
        host: Arc<dyn MonitorHost>,
        council: Arc<dyn Council>,
    ) -> Self {
        let pub_ = Publisher::new(cfg.bus());
        Self {
            cfg,
            runs: HashMap::new(),
            units: HashMap::new(),
            host,
            council,
            pub_,
            unknown: HashMap::new(),
            ended: HashSet::new(),
            helped: HashSet::new(),
        }
    }

    /// Arm `run` from its persisted team state (P1's replay set): its floor is its `path.started`.
    pub fn arm(&mut self, run: &LiveTeamRun) {
        let Some(floor) = run.team.stream_floor else {
            return;
        };
        self.unknown.remove(&run.run_id);
        self.runs.entry(run.run_id.clone()).or_insert(RunState {
            floor,
            // The engine's own record of the seats (the PA first): the members' candidates even
            // when `path.started` has aged off the bus. `path.started`, when read, restates them.
            pa: run.roster.first().cloned().unwrap_or_default(),
            roster: run.roster.clone(),
            // Unknown until `path.started` is read: a missing one is the gap (§4.7).
            gap: true,
            ..Default::default()
        });
    }

    /// The runs armed but not yet read from their floor, with their floors.
    pub fn floors(&self) -> Vec<(String, i64)> {
        self.runs
            .iter()
            .map(|(r, s)| (r.clone(), s.floor))
            .collect()
    }

    /// Consume one row. `replay` = read below the spawn tail `T` (history: nothing is started for
    /// it). Returns the jobs the row makes due. Idempotent: rows are applied by entity id, so a
    /// row seen both in replay and live changes nothing the second time.
    pub fn on_row(&mut self, row: &TeamRow, replay: bool) -> Vec<Job> {
        let run_id = row.event.env.run_id.clone();
        let env = row.event.env.clone();
        if let TeamBody::PathStarted(b) = &row.event.body {
            if self.ended.contains(&run_id) {
                return Vec::new();
            }
            self.unknown.remove(&run_id);
            let st = self.runs.entry(run_id.clone()).or_default();
            if st.floor == 0 || row.event_id <= st.floor {
                st.floor = row.event_id;
            }
            st.pa = b.cli.clone();
            st.roster = b.roster.clone();
            st.gap = false;
            return Vec::new();
        }
        if !self.runs.contains_key(&run_id) {
            // Asked about (rate-limited, `ARM_RETRY`) until the engine lists it or it ends: a
            // run resumed from a pause starts publishing again and is armed then.
            if !self.ended.contains(&run_id) {
                self.unknown.entry(run_id).or_insert(None);
            }
            return Vec::new();
        }
        let boot_ms = self.cfg.boot_ms;
        let key = |env: &Envelope| -> Option<(u32, u32)> { Some((env.ord?, env.attempt?)) };
        match &row.event.body {
            TeamBody::PlanProposed(b) => {
                let st = self.runs.get_mut(&run_id).expect("armed");
                st.asked = b.monitors.asked;
            }
            TeamBody::PlanAccepted(b) => {
                let st = self.runs.get_mut(&run_id).expect("armed");
                st.band = (!b.band.trim().is_empty()).then(|| b.band.clone());
            }
            TeamBody::PlanRevised(b) => {
                let st = self.runs.get_mut(&run_id).expect("armed");
                if !b.to_band.trim().is_empty() {
                    st.band = Some(b.to_band.clone());
                }
            }
            TeamBody::StepClaimed(b) => {
                let Some(k) = key(&env) else {
                    return Vec::new();
                };
                let st = self.runs.get_mut(&run_id).expect("armed");
                // Live = claimed by THIS process (its runner is alive to wait for a fold): the
                // claim's own time against this boot, never whether the row was read in replay —
                // a claim from before the boot is dead however late the tail was read.
                let live = env.at >= boot_ms;
                let seen = st.attempts.entry(k).or_default();
                if seen.claimed_id != 0 {
                    return Vec::new();
                }
                seen.claimed_id = row.event_id;
                seen.claimed_at = env.at;
                seen.by = env.by.clone();
                seen.live = live;
                if live {
                    return self.attach(&run_id, k, row.event_id, &env, b);
                }
            }
            TeamBody::CheckpointReached(b) => {
                if let Some(k) = key(&env) {
                    let uk = (run_id.clone(), k.0, k.1);
                    if let Some(u) = self.units.get(&uk) {
                        let mut u = u.lock().unwrap_or_else(|p| p.into_inner());
                        u.last_seq = u.last_seq.max(b.seq);
                        u.titles.push(b.title.clone());
                        if kind_may_change_tree(&b.kind) {
                            u.pending = true;
                            for m in &mut u.monitors {
                                m.pending = true;
                            }
                        }
                    }
                }
            }
            TeamBody::FindingRaised(b) => {
                if let Some(k) = key(&env) {
                    let st = self.runs.get_mut(&run_id).expect("armed");
                    let seen = st.attempts.entry(k).or_default();
                    if !seen.raised.iter().any(|(_, f)| f.raise_seq == b.raise_seq) {
                        seen.raised.push((env.clone(), b.clone()));
                    }
                }
            }
            TeamBody::AdviceAnswered(b) => {
                if let Some(k) = key(&env) {
                    let st = self.runs.get_mut(&run_id).expect("armed");
                    let seen = st.attempts.entry(k).or_default();
                    if b.disposition == tev::AdviceDisposition::Accepted {
                        seen.accepted.insert(b.finding_id.clone());
                    } else {
                        seen.accepted.remove(&b.finding_id);
                    }
                }
            }
            TeamBody::FindingSettled(b) => {
                if let Some(k) = key(&env) {
                    if b.status != SettledStatus::Held {
                        let st = self.runs.get_mut(&run_id).expect("armed");
                        st.attempts
                            .entry(k)
                            .or_default()
                            .closed
                            .insert(b.finding_id.clone());
                    }
                }
            }
            TeamBody::HelpRequested(b) => {
                let Some(k) = key(&env) else {
                    return Vec::new();
                };
                let live = self
                    .runs
                    .get(&run_id)
                    .and_then(|st| st.attempts.get(&k))
                    .is_some_and(|a| a.live);
                if replay && !live || !self.helped.insert(b.help_id.clone()) {
                    return Vec::new();
                }
                return self.help_job(&run_id, k, &env, b).into_iter().collect();
            }
            TeamBody::StepCompleted(b) => {
                let Some(k) = key(&env) else {
                    return Vec::new();
                };
                let st = self.runs.get_mut(&run_id).expect("armed");
                let seen = st.attempts.entry(k).or_default();
                if seen.completed {
                    return Vec::new();
                }
                seen.completed = true;
                // An attempt of this process whose claim the supervisor never read (it cannot
                // happen with a whole stream) still gets a fold — `stream_gap`, which pauses —
                // rather than none, so its runner is not left to its timeout.
                let unseen = seen.claimed_id == 0 && env.at >= boot_ms;
                if !seen.live && !unseen {
                    return Vec::new();
                }
                let claimed_id = seen.claimed_id;
                let uk = (run_id.clone(), k.0, k.1);
                let unit = match self.units.remove(&uk) {
                    Some(u) => u,
                    None if unseen => Arc::new(Mutex::new(UnitTeam::new(
                        AttachCtx {
                            run_id: run_id.clone(),
                            ord: k.0,
                            attempt: k.1,
                            creator: env.by.clone(),
                            plan: TeamPlan::default(),
                            repo: None,
                            baseline_tree: None,
                            criterion: String::new(),
                            phase: String::new(),
                            step_id: b.step_id.clone(),
                            code_graph_db: None,
                        },
                        self.pub_.clone(),
                    ))),
                    None => return Vec::new(),
                };
                let gap = unseen
                    || st.gap && k.1 > 0 && !st.attempts.keys().any(|(o, a)| *o == k.0 && *a < k.1);
                return vec![Job::FinalPass(Box::new(FinalPassJob {
                    unit,
                    ok: b.status == StepCompletion::Ok,
                    completed_at: env.at,
                    claimed_id,
                    gap,
                    budget: self.cfg.final_pass_budget,
                    limits: self.cfg.limits,
                    max_disputes: self.cfg.max_disputes,
                    bus_db: self.cfg.bus_db.clone(),
                    members: st
                        .attempts
                        .iter()
                        .filter(|((o, _), _)| *o == k.0)
                        .map(|((_, a), s)| (*a, s.by.clone()))
                        .collect(),
                    pa: st.pa.clone(),
                }))];
            }
            TeamBody::LedgerFolded(_) => {
                if let Some(k) = key(&env) {
                    let st = self.runs.get_mut(&run_id).expect("armed");
                    st.attempts.entry(k).or_default().folded = true;
                }
            }
            TeamBody::PathEnded(_) => {
                self.forget(&run_id);
                self.ended.insert(run_id);
            }
            _ => {}
        }
        Vec::new()
    }

    /// Start watching a live attempt: its members, and the dead attempts' findings it carries.
    fn attach(
        &mut self,
        run_id: &str,
        k: (u32, u32),
        claimed_id: i64,
        env: &Envelope,
        b: &tev::StepClaimed,
    ) -> Vec<Job> {
        let st = self.runs.get(run_id).expect("armed");
        let repo = match (&b.repo, &b.baseline_tree) {
            (Some(r), Some(_)) if !r.git_dir.is_empty() => Some(Repo {
                workdir: PathBuf::from(&r.workdir),
                git_dir: PathBuf::from(&r.git_dir),
            }),
            _ => None,
        };
        let ctx = AttachCtx {
            run_id: run_id.to_string(),
            ord: k.0,
            attempt: k.1,
            creator: env.by.clone(),
            plan: st.plan(),
            repo,
            baseline_tree: b.baseline_tree.clone(),
            criterion: b.criterion.clone(),
            phase: b.phase.clone(),
            step_id: b.step_id.clone(),
            code_graph_db: b.code_graph_db.clone(),
        };
        // The findings a dead earlier attempt of this unit left unresolved (§4.7 step 3).
        let carried: Vec<(u32, Envelope, FindingRaised)> = st
            .attempts
            .iter()
            // Any dead earlier attempt of the unit, folded or not: a fold the dead process
            // published may never have reached its gate.
            .filter(|((o, a), s)| *o == k.0 && *a < k.1 && !s.live)
            .flat_map(|((_, a), s)| s.unresolved().into_iter().map(move |(e, f)| (*a, e, f)))
            .collect();
        let _ = claimed_id;
        let mut unit = UnitTeam::new(ctx, self.pub_.clone());
        for (from, e, f) in carried {
            unit.carry(super::runner::finding_of(&e, &f), from);
        }
        self.units
            .insert((run_id.to_string(), k.0, k.1), Arc::new(Mutex::new(unit)));
        Vec::new()
    }

    fn help_job(
        &self,
        run_id: &str,
        k: (u32, u32),
        env: &Envelope,
        b: &tev::HelpRequested,
    ) -> Option<Job> {
        let st = self.runs.get(run_id)?;
        let uk = (run_id.to_string(), k.0, k.1);
        // Prefer a member already watching the attempt, then the roster; never the asker or the PA.
        let watching: Vec<String> = self
            .units
            .get(&uk)
            .map(|u| {
                u.lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .monitors
                    .iter()
                    .filter(|m| m.state != SlotState::Failed)
                    .map(|m| m.seat.clone())
                    .collect()
            })
            .unwrap_or_default();
        let candidates: Vec<String> = watching
            .into_iter()
            .chain(st.roster.iter().cloned())
            .filter(|s| *s != env.by && *s != st.pa)
            .collect();
        let workdir = self.units.get(&uk).and_then(|u| {
            u.lock()
                .unwrap_or_else(|p| p.into_inner())
                .ctx
                .repo
                .as_ref()
                .map(|r| r.workdir.to_string_lossy().into_owned())
        });
        Some(Job::Help(HelpJob {
            key: uk,
            help_id: b.help_id.clone(),
            question: b.question.clone(),
            context: b.context.clone(),
            candidates,
            read_roots: workdir.into_iter().collect(),
            budget: self.cfg.limits.monitor_turn_budget,
        }))
    }

    /// Forget a run: close its members' sessions.
    fn forget(&mut self, run_id: &str) {
        let keys: Vec<UnitKey> = self
            .units
            .keys()
            .filter(|(r, _, _)| r == run_id)
            .cloned()
            .collect();
        for k in keys {
            if let Some(u) = self.units.remove(&k) {
                let u = u.lock().unwrap_or_else(|p| p.into_inner());
                for m in &u.monitors {
                    if m.state != SlotState::Pending {
                        self.host.close(&m.pool_key);
                    }
                }
            }
        }
        self.runs.remove(run_id);
        self.unknown.remove(run_id);
    }

    /// The batches due at `now` (DES-001 §4.3): a member with a pending tree change, no batch in
    /// flight, budget left, and at least `batch_min_interval` since its previous batch started.
    /// Members are summoned here — lazily, when the first batch is due.
    pub fn due_batches(&mut self, now: Instant) -> Vec<Job> {
        let mut jobs = Vec::new();
        for unit in self.units.values() {
            let mut u = unit.lock().unwrap_or_else(|p| p.into_inner());
            if !u.summoned && u.pending {
                u.summon(&*self.host);
            }
            let mut took_titles = false;
            for i in 0..u.monitors.len() {
                let limits = self.cfg.limits;
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
                jobs.push(Job::Batch(Box::new(u.job(
                    i,
                    None,
                    limits.monitor_turn_budget,
                    limits.diff_cap,
                ))));
                took_titles = true;
            }
            if took_titles {
                u.titles.clear();
            }
        }
        jobs
    }

    /// Fold a finished batch back in. A batch for an attempt already handed to its final pass is
    /// dropped (the final batch reviews the settled tree).
    pub fn apply_batch(&mut self, done: BatchDone) {
        if let Some(unit) = self.units.get(&done.key) {
            unit.lock().unwrap_or_else(|p| p.into_inner()).apply(done);
        }
    }

    pub fn host(&self) -> Arc<dyn MonitorHost> {
        Arc::clone(&self.host)
    }

    pub fn council(&self) -> Arc<dyn Council> {
        Arc::clone(&self.council)
    }

    pub fn publisher_bus(&self) -> TeamBus {
        self.pub_.bus.clone()
    }

    /// Retry the S lanes that hold spooled facts, in order (the lane drains FIFO), until each is
    /// empty or its bound has run out — then its lines stay for `replay_team_outbox`.
    fn retry_spooled(&self) {
        // Folds past their deadline die BEFORE any lane drains, so no drain publishes them late.
        self.pub_.expire_folds();
        let due: Vec<(String, Instant)> = self
            .pub_
            .spooled
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .iter()
            .map(|(r, t)| (r.clone(), *t))
            .collect();
        for (run, since) in due {
            let lane = super::publish::Lane {
                owner: super::publish::LaneOwner::Supervisor,
                run_id: run.clone(),
                attempt: None,
            };
            let report = self.pub_.bus.drain_lane(&lane);
            let over = since.elapsed() >= self.cfg.publish_bound;
            if report.remaining == 0 || over {
                if over && report.remaining > 0 {
                    eprintln!(
                        "wicked-core: team supervisor: {run}'s spooled facts outlived the bound; \
                         they stay for replay_team_outbox"
                    );
                }
                self.pub_
                    .spooled
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .remove(&run);
            }
        }
    }

    /// The unknown runs due for an "are you live?" question.
    fn unknown_due(&mut self, now: Instant) -> Vec<String> {
        let mut due = Vec::new();
        for (run, asked) in self.unknown.iter_mut() {
            if asked.is_none_or(|t| now.duration_since(t) >= ARM_RETRY) {
                *asked = Some(now);
                due.push(run.clone());
            }
        }
        due
    }
}

// ── Help (DES-002 §8.8 "Support") ───────────────────────────────────────────────────────────────

/// A member's turn answering one `HELP:` question.
pub struct HelpJob {
    key: UnitKey,
    help_id: String,
    question: String,
    context: String,
    candidates: Vec<String>,
    read_roots: Vec<String>,
    budget: Duration,
}

/// Run a help turn: the first admitted candidate answers on a fresh read-only session, and its
/// answer is published as `help.answered` (S). No admitted member, or a failed turn: nothing is
/// published — the question stays unanswered on the stream, and the PA was told only members
/// answer (a human-directed question is S1 elicitation).
pub fn run_help(job: &HelpJob, host: &dyn MonitorHost, pub_: &TeamBus) {
    let Some(seat) = job
        .candidates
        .iter()
        .find(|s| host.admitted(s).is_ok())
        .cloned()
    else {
        eprintln!(
            "wicked-core: team: help {} of {}:{}:{} has no admitted member to answer it",
            job.help_id, job.key.0, job.key.1, job.key.2
        );
        return;
    };
    let pool_key = format!(
        "team:{}:{}:{}:help:{}",
        job.key.0, job.key.1, job.key.2, job.help_id
    );
    let scope = MonitorScope {
        cwd: crate::acp_runner::ChatScope::scratch_for(&pool_key),
        code_graph_db: None,
        read_roots: job.read_roots.clone(),
    };
    if let Err(e) = host.open(&pool_key, &seat, &scope) {
        eprintln!("wicked-core: team: help member {seat} did not open ({e})");
        return;
    }
    let prompt = format!(
        "[wicked-core · team member · READ-ONLY]\n\
         The agent working this step asks the team for help. Answer it from the code, concisely, \
         with evidence. You advise; you do not decide.\n\
         Question: {}\nContext: {}\n\
         Reply with your answer, then one line `EVIDENCE: <path:line>` per citation, then DONE.\n",
        job.question, job.context
    );
    let reply = host.turn(&pool_key, &prompt, job.budget);
    host.close(&pool_key);
    let reply = match reply {
        Ok(r) => r,
        Err(e) => {
            eprintln!("wicked-core: team: help member {seat} turn failed ({e})");
            return;
        }
    };
    let (answer, evidence) = super::parse_help_answer(&reply);
    if answer.is_empty() {
        return;
    }
    let answer_id = format!(
        "t-{}",
        crate::bus::deterministic_key(&[&job.key.0, &job.help_id, &seat])
    );
    let ev = TeamEvent {
        env: env_of(
            &job.key,
            &seat,
            Some(format!("help.requested#{}", job.help_id)),
        ),
        body: TeamBody::HelpAnswered(HelpAnswered {
            help_id: job.help_id.clone(),
            answer_id,
            answer,
            evidence,
        }),
    };
    let _ = Publisher::new(pub_.clone()).publish(&ev);
}

// ── The final pass (DES-001 §4.7, DES-002 §8.11) ────────────────────────────────────────────────

/// One attempt's final pass, handed off the cursor thread.
pub struct FinalPassJob {
    unit: Arc<Mutex<UnitTeam>>,
    ok: bool,
    /// The attempt's `step.completed.at`: the deadline is measured from it.
    completed_at: i64,
    claimed_id: i64,
    /// The attempt's earlier rows are gone (§4.7): its ledger is `stream_gap`.
    gap: bool,
    budget: Duration,
    limits: TeamLimits,
    max_disputes: usize,
    bus_db: String,
    /// Every attempt of the unit and who ran it (the member of a reviewed step).
    members: BTreeMap<u32, String>,
    pa: String,
}

impl FinalPassJob {
    pub fn key(&self) -> UnitKey {
        self.unit.lock().unwrap_or_else(|p| p.into_inner()).key()
    }
}

/// Whether the pass still has time, against a wall-clock deadline.
struct Clock {
    work_until: i64,
    publish_until: i64,
}

impl Clock {
    fn left(&self) -> Duration {
        Duration::from_millis((self.work_until - now_ms()).max(0) as u64)
    }
    fn expired(&self) -> bool {
        now_ms() >= self.work_until
    }
    fn can_publish(&self) -> bool {
        now_ms() < self.publish_until
    }
}

/// The attempt's rows, from its `step.claimed` on.
fn attempt_rows(
    bus_db: &str,
    key: &UnitKey,
    claimed_id: i64,
) -> anyhow::Result<(Vec<TeamRow>, usize)> {
    #[cfg(test)]
    if tests::take_injected_read_failure() {
        anyhow::bail!("injected attempt_rows read failure (test)");
    }
    let db = BusDb::shared(bus_db)?;
    let mut floor = claimed_id.saturating_sub(1);
    let mut rows = Vec::new();
    let mut malformed = 0usize;
    loop {
        let batch = db.poll(TEAM_FILTER, floor, READ_BATCH)?;
        let n = batch.len();
        for ev in batch {
            floor = floor.max(ev.event_id);
            if ev.payload.get("run_id").and_then(Value::as_str) != Some(key.0.as_str()) {
                continue;
            }
            let (Some(o), Some(a)) = (
                ev.payload.get("ord").and_then(Value::as_u64),
                ev.payload.get("attempt").and_then(Value::as_u64),
            ) else {
                continue;
            };
            if o as u32 != key.1 || a as u32 != key.2 {
                continue;
            }
            match TeamEvent::from_payload(&ev.event_type, &ev.payload) {
                Ok(event) => rows.push(TeamRow {
                    event_id: ev.event_id,
                    event,
                }),
                Err(_) => malformed += 1,
            }
        }
        if n < READ_BATCH {
            return Ok((rows, malformed));
        }
    }
}

fn close_all(u: &mut UnitTeam, host: &dyn MonitorHost) {
    let key = u.key();
    let monitors = u.ledger_monitors();
    for (i, m) in u.monitors.clone().iter().enumerate() {
        if m.state == SlotState::Pending && m.open_seq == 0 {
            continue;
        }
        if m.state != SlotState::Pending {
            host.close(&m.pool_key);
        }
        if m.open_seq > 0 {
            let lm = &monitors[i];
            let ev = TeamEvent {
                env: env_of(&key, &m.seat, None),
                body: TeamBody::MemberLeft(MemberLeft {
                    member_id: m.id.clone(),
                    open_seq: m.open_seq,
                    seat: m.seat.clone(),
                    status: lm.status,
                    batches: lm.batches,
                    error: lm.error.clone(),
                }),
            };
            u.publish(&ev);
        }
    }
}

/// The worker's state on one unaccepted finding, as the hold round and the council read it.
fn worker_position(
    f: &super::LedgerFinding,
    deliveries: &BTreeMap<u32, String>,
    seq: u32,
) -> String {
    match f.status {
        FindingStatus::Declined => f
            .worker_reason
            .clone()
            .filter(|r| !r.trim().is_empty())
            .unwrap_or_else(|| "declined with no reason".into()),
        _ if f.delivery == super::LedgerDelivery::Injected => "no answer".into(),
        _ => format!(
            "not delivered — {}",
            deliveries
                .get(&seq)
                .cloned()
                .unwrap_or_else(|| "no delivery on record".into())
        ),
    }
}

/// Run one attempt's final pass and publish its `ledger.folded` (or tombstone it past its
/// deadline). Every S fact it publishes goes through the one wrapper.
pub fn run_final_pass(job: FinalPassJob, host: &dyn MonitorHost, council: &dyn Council) {
    let margin = margin_of(job.budget);
    let clock = Clock {
        work_until: job.completed_at + (job.budget.saturating_sub(margin)).as_millis() as i64,
        publish_until: job.completed_at + job.budget.as_millis() as i64,
    };
    let key = job.key();
    let mut timed_out = false;
    let mut final_lines: BTreeMap<u32, u32> = BTreeMap::new();
    let s_lane = super::publish::Lane {
        owner: super::publish::LaneOwner::Supervisor,
        run_id: key.0.clone(),
        attempt: None,
    };
    let bus = job
        .unit
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pub_
        .bus
        .clone();
    // What this supervisor spooled for the run lands first (FIFO), so every read below sees it.
    let drain = || {
        bus.drain_lane(&s_lane);
    };
    drain();

    // 1–3 (S2): the final batch per member over the settled tree, and re-confirmation.
    if job.ok {
        let prepared = {
            let mut u = job.unit.lock().unwrap_or_else(|p| p.into_inner());
            match u.ctx.repo.clone() {
                None => {
                    u.summon(host);
                    None
                }
                Some(repo) => match repo.snapshot() {
                    Err(e) => {
                        for m in &mut u.monitors {
                            m.error = Some(format!("final snapshot failed: {e}"));
                        }
                        None
                    }
                    Ok(t_final) => {
                        u.summon(host);
                        let mut jobs = Vec::new();
                        for i in 0..u.monitors.len() {
                            let m = &mut u.monitors[i];
                            if m.state == SlotState::Failed {
                                continue;
                            }
                            if m.batches >= job.limits.max_batches {
                                m.budget_exhausted = true;
                                continue;
                            }
                            m.in_flight = true;
                            jobs.push(u.job(
                                i,
                                Some(t_final.clone()),
                                job.limits.monitor_turn_budget,
                                job.limits.diff_cap,
                            ));
                        }
                        Some((repo, t_final, jobs))
                    }
                },
            }
        };
        if let Some((repo, t_final, jobs)) = prepared {
            for mut b in jobs {
                if clock.expired() {
                    timed_out = true;
                    let mut u = job.unit.lock().unwrap_or_else(|p| p.into_inner());
                    u.monitors[b.slot].in_flight = false;
                    u.monitors[b.slot].timed_out = true;
                    continue;
                }
                b.budget = b.budget.min(clock.left());
                let done = run_job(&b, host);
                job.unit
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .apply(done);
            }
            let mut u = job.unit.lock().unwrap_or_else(|p| p.into_inner());
            let mut superseded = Vec::new();
            for (i, f) in u.book.findings.iter().enumerate() {
                let text = repo.file(&t_final, &f.finding.path);
                let seq = u.book.raises[i];
                match locate(text.as_deref(), f.finding.line, &f.finding.evidence) {
                    Some(n) => {
                        final_lines.insert(seq, n);
                    }
                    None => superseded.push((seq, f.finding.clone())),
                }
            }
            for (seq, f) in superseded {
                let ev = TeamEvent {
                    env: env_of(&key, &f.seat, Some(format!("finding.raised#{seq}"))),
                    body: TeamBody::FindingSettled(FindingSettled {
                        raise_seq: seq,
                        finding_id: f.finding_id.clone(),
                        status: SettledStatus::Superseded,
                        reason: format!(
                            "its evidence text is gone from the settled tree {t_final}"
                        ),
                        final_line: None,
                    }),
                };
                u.publish(&ev);
            }
        }
    }

    // 5 (S3): the hold round over the stream's view of the worker's answers.
    let read = || {
        drain();
        attempt_rows(&job.bus_db, &key, job.claimed_id)
    };
    if job.ok {
        match read() {
            Ok((rows, _)) => {
                let ledger = tev::fold(&rows);
                let deliveries = delivery_outcomes(&rows);
                let mut u = job.unit.lock().unwrap_or_else(|p| p.into_inner());
                hold_round(
                    &mut u,
                    &ledger,
                    &deliveries,
                    host,
                    &clock,
                    &final_lines,
                    &mut timed_out,
                );
            }
            Err(e) => eprintln!(
                "wicked-core: team: {}:{}:{} hold round could not read the stream ({e:#})",
                key.0, key.1, key.2
            ),
        }
    }

    // 6 (S6): one council per unresolved HIGH, and per held member-step rejection.
    let mut disputes = 0usize;
    let mut step_answers: BTreeMap<String, (bool, String)> = BTreeMap::new();
    if job.ok {
        if let Ok((rows, _)) = read() {
            let ledger = tev::fold(&rows);
            let deliveries = delivery_outcomes(&rows);
            let mut u = job.unit.lock().unwrap_or_else(|p| p.into_inner());
            for f in tev::unresolved_highs(&ledger) {
                if f.dispute.is_some() {
                    continue;
                }
                let Some(seq) = u.book.raise_of(&f.finding.finding_id) else {
                    continue;
                };
                disputes += 1;
                let transcript: Vec<i64> = rows
                    .iter()
                    .filter(|r| names_finding(&r.event.body, seq, &f.finding.finding_id))
                    .map(|r| r.event_id)
                    .collect();
                let worker = worker_position(f, &deliveries, seq);
                let declined = f.status == FindingStatus::Declined;
                let question = if declined {
                    "The worker declined this HIGH finding and the monitor holds it. Should the run \
                     continue autonomously with the worker's refusal standing? YES = continue. NO \
                     = a human must decide."
                } else {
                    "The worker did not accept this HIGH finding (gave no answer / never received \
                     it) and the monitor holds it. Should the run continue autonomously with the \
                     work as submitted? YES = continue. NO = a human must decide."
                };
                let mut parties = vec![u.ctx.creator.clone(), f.finding.seat.clone()];
                parties.extend(f.corroborated_by.iter().cloned());
                let monitor_reason = f
                    .monitor_reply
                    .as_ref()
                    .map(|r| r.reason.clone())
                    .unwrap_or_else(|| "no reply (counted as hold)".into());
                let positions = vec![
                    CouncilPosition {
                        by: format!("worker {}", u.ctx.creator),
                        position: if declined {
                            "YES — the refusal stands".into()
                        } else {
                            "YES — the work as submitted stands".into()
                        },
                        reason: worker,
                    },
                    CouncilPosition {
                        by: format!("monitor {}", f.finding.seat),
                        position: "NO — the finding stands".into(),
                        reason: monitor_reason,
                    },
                ];
                let evidence = council_evidence(&u.ctx, f, final_lines.get(&seq).copied());
                let subject = tev::subject_finding(seq);
                convene_one(
                    &mut u,
                    council,
                    &clock,
                    Convening {
                        subject,
                        finding_id: Some(f.finding.finding_id.clone()),
                        trigger: CouncilTrigger::UnresolvedHigh,
                        question: question.into(),
                        positions,
                        evidence,
                        parties,
                        transcript,
                        re: format!("finding.settled#{seq}"),
                        over_cap: disputes > job.max_disputes,
                    },
                    &mut timed_out,
                );
            }
            // Member steps the PA rejected in this attempt: the member may HOLD (§8.8).
            for r in &ledger.step_reviews {
                if r.verdict != StepVerdict::Rejected {
                    continue;
                }
                let Some(member) = job.members.get(&r.reviewed_attempt).cloned() else {
                    continue;
                };
                if member == job.pa {
                    continue;
                }
                let answer = member_answer(&u, host, &member, r, &clock);
                let Some((held, why)) = answer else {
                    continue;
                };
                step_answers.insert(r.step_id.clone(), (held, why.clone()));
                if !held {
                    continue;
                }
                disputes += 1;
                let subject = tev::subject_step(&r.step_id, r.reviewed_attempt);
                let transcript: Vec<i64> = rows
                    .iter()
                    .filter(|x| matches!(&x.event.body, TeamBody::StepReviewed(b) if b.step_id == r.step_id))
                    .map(|x| x.event_id)
                    .collect();
                convene_one(
                    &mut u,
                    council,
                    &clock,
                    Convening {
                        subject,
                        finding_id: None,
                        trigger: CouncilTrigger::MemberStep,
                        question: "The PA rejected this step; the member holds its output. Should \
                                   the member's output count as submitted?"
                            .into(),
                        positions: vec![
                            CouncilPosition {
                                by: format!("member {member}"),
                                position: "YES — the output counts".into(),
                                reason: why,
                            },
                            CouncilPosition {
                                by: format!("pa {}", job.pa),
                                position: "NO — the rejection stands".into(),
                                reason: r.reason.clone(),
                            },
                        ],
                        evidence: cap_utf8(
                            &format!(
                                "Step {} (attempt {}) by {member}; the PA's rejection: {}",
                                r.step_id, r.reviewed_attempt, r.reason
                            ),
                            16 * 1024,
                        ),
                        parties: vec![member.clone(), job.pa.clone()],
                        transcript,
                        re: format!("step.reviewed#{}", r.step_id),
                        over_cap: disputes > job.max_disputes,
                    },
                    &mut timed_out,
                );
            }
        }
    }

    // 7: close the members, fold the attempt's rows, overlay S's own record, publish.
    let (ledger, transcript) = {
        let mut u = job.unit.lock().unwrap_or_else(|p| p.into_inner());
        close_all(&mut u, host);
        match read() {
            Ok((rows, malformed)) => {
                let mut l = tev::fold(&rows);
                overlay(&mut l, &u, &final_lines, &step_answers);
                // A fact of this attempt that never reached the outbox (`lost`), or one still
                // waiting in it, is not in the rows: the record is incomplete.
                bus.drain_lane(&s_lane);
                let spooled = bus.lane_has_pending(&s_lane);
                if malformed > 0
                    || job.gap
                    || u.lost
                    || spooled
                    || l.final_pass == FinalPass::StreamGap
                {
                    // An incomplete record pauses, never auto-approves (§4.7).
                    l.final_pass = FinalPass::StreamGap;
                    l.refresh_pause();
                } else if timed_out {
                    l = tev::synthesize_timeout(l);
                }
                (
                    l,
                    super::runner::transcript_of(&rows, super::runner::TRANSCRIPT_CAP),
                )
            }
            Err(e) => {
                eprintln!(
                    "wicked-core: team: {}:{}:{} could not read its stream to fold ({e:#}); the \
                     ledger is stream_gap",
                    key.0, key.1, key.2
                );
                (
                    TeamLedger::new(
                        FinalPass::StreamGap,
                        u.ledger_monitors(),
                        Vec::new(),
                        u.book.rejected,
                    ),
                    super::runner::transcript_of(&[], super::runner::TRANSCRIPT_CAP),
                )
            }
        }
    };
    let ev = TeamEvent {
        env: env_of(&key, "engine", None),
        body: TeamBody::LedgerFolded(LedgerFolded {
            final_pass: ledger.final_pass,
            ledger,
            transport: Transport::Bus,
            transcript,
        }),
    };
    let fold_key = ev.key().ok();
    if !clock.can_publish() {
        supersede_fold(
            &job.unit,
            fold_key.as_deref(),
            "ledger.folded past its deadline",
        );
        return;
    }
    match bus.publish(&ev) {
        Ok(PublishOutcome::Published(_)) | Ok(PublishOutcome::Superseded) => {}
        // Spooled behind the lane (or unwritable): the cursor loop retries the lane within its
        // bound and tombstones the fold at its deadline if it is still waiting then.
        Ok(PublishOutcome::Spooled(_)) | Err(_) => {
            let u = job.unit.lock().unwrap_or_else(|p| p.into_inner());
            u.pub_
                .spooled
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .entry(key.0.clone())
                .or_insert_with(Instant::now);
            if let Some(k) = fold_key {
                u.pub_
                    .folds
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push((k, clock.publish_until));
            }
        }
    }
}

/// Tombstone a fold that missed its deadline (§4.1, §8.11): a late replay never publishes a
/// ledger the gate did not use.
fn supersede_fold(unit: &Arc<Mutex<UnitTeam>>, key: Option<&str>, why: &str) {
    let Some(key) = key else {
        return;
    };
    let bus = unit
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .pub_
        .bus
        .clone();
    if let Err(e) = bus.supersede_fact(key, why) {
        eprintln!("wicked-core: team: the late fold's tombstone was not written ({e})");
    }
}

/// Whether `body` is one of the rows about finding `seq` (`finding_id`) — the council's transcript.
fn names_finding(body: &TeamBody, seq: u32, id: &str) -> bool {
    match body {
        TeamBody::FindingRaised(b) => b.raise_seq == seq,
        TeamBody::AdviceDelivered(b) => b.raise_seq == seq && b.finding_id == id,
        TeamBody::AdviceAnswered(b) => b.raise_seq == seq && b.finding_id == id,
        TeamBody::FindingSettled(b) => b.raise_seq == seq && b.finding_id == id,
        _ => false,
    }
}

/// The latest delivery outcome per `raise_seq` (its channel and outcome, for "not delivered — …").
fn delivery_outcomes(rows: &[TeamRow]) -> BTreeMap<u32, String> {
    let mut out = BTreeMap::new();
    for r in rows {
        if let TeamBody::AdviceDelivered(b) = &r.event.body {
            if b.outcome != tev::DeliveryOutcome::Injected {
                out.insert(
                    b.raise_seq,
                    format!("{} ({})", b.outcome.as_str(), b.channel.as_str()),
                );
            }
        }
    }
    out
}

/// DES-001 §4.7 step 5: one turn per member with ≥1 unaccepted finding, listing each with the
/// worker's state; `finding.settled{held|withdrawn}` for every unaccepted finding — exactly one
/// each — and silence (no line, a failed turn, no time left) is HOLD.
fn hold_round(
    u: &mut UnitTeam,
    ledger: &TeamLedger,
    deliveries: &BTreeMap<u32, String>,
    host: &dyn MonitorHost,
    clock: &Clock,
    final_lines: &BTreeMap<u32, u32>,
    timed_out: &mut bool,
) {
    let key = u.key();
    // The unaccepted findings this attempt raised, by the member that authored them.
    let mut by_member: BTreeMap<String, Vec<(u32, super::LedgerFinding)>> = BTreeMap::new();
    for f in &ledger.findings {
        if !tev::is_unaccepted(f) || f.monitor_reply.is_some() {
            continue;
        }
        let Some(seq) = u.book.raise_of(&f.finding.finding_id) else {
            continue;
        };
        by_member
            .entry(f.finding.monitor_id.clone())
            .or_default()
            .push((seq, f.clone()));
    }
    for (member_id, items) in by_member {
        let slot = u.monitors.iter().position(|m| m.id == member_id);
        let replies = match slot {
            Some(i) if u.monitors[i].state != SlotState::Failed && !clock.expired() => {
                let m = u.monitors[i].clone();
                let mut prompt = String::from(
                    "[wicked-core · team member · hold round]\nThe worker's turn has ended. For \
                     each finding below the worker did not accept, answer `HOLD <findingId> — \
                     <reason>` to stand by it or `WITHDRAW <findingId> — <reason>` to drop it, \
                     then DONE.\n",
                );
                for (seq, f) in &items {
                    prompt.push_str(&format!(
                        "- {} [{}] {}:{} — {} · worker: {}\n",
                        f.finding.finding_id,
                        f.finding.severity.as_str().to_ascii_uppercase(),
                        f.finding.path,
                        final_lines.get(seq).copied().unwrap_or(f.finding.line),
                        cap_utf8(&f.finding.claim, 512),
                        worker_position(f, deliveries, *seq)
                    ));
                }
                let opened = if m.state == SlotState::Pending {
                    host.open(&m.pool_key, &m.seat, &u.scope(&m.id)).is_ok()
                } else {
                    true
                };
                if opened {
                    let budget = clock.left().min(super::MONITOR_TURN_BUDGET);
                    host.turn(&m.pool_key, &prompt, budget)
                        .map(|r| super::parse_hold_lines(&r))
                        .unwrap_or_default()
                } else {
                    BTreeMap::new()
                }
            }
            _ => {
                if clock.expired() {
                    *timed_out = true;
                }
                BTreeMap::new()
            }
        };
        for (seq, f) in items {
            let reply = replies.get(&f.finding.finding_id).cloned();
            let (status, reason) = match reply {
                Some(r) if r.kind == ReplyKind::Withdraw => (SettledStatus::Withdrawn, r.reason),
                Some(r) => (SettledStatus::Held, r.reason),
                None => (
                    SettledStatus::Held,
                    "no reply (counted as hold)".to_string(),
                ),
            };
            let ev = TeamEvent {
                env: env_of(&key, &f.finding.seat, Some(format!("finding.raised#{seq}"))),
                body: TeamBody::FindingSettled(FindingSettled {
                    raise_seq: seq,
                    finding_id: f.finding.finding_id.clone(),
                    status,
                    reason,
                    final_line: final_lines.get(&seq).copied(),
                }),
            };
            u.publish(&ev);
        }
    }
}

/// The council's evidence for one finding (DES-001 §6.3): the finding, `path:finalLine`, the
/// settled hunk around it, the criterion and the tree — capped at 16 KB.
fn council_evidence(ctx: &AttachCtx, f: &super::LedgerFinding, final_line: Option<u32>) -> String {
    let mut s = format!(
        "Finding {} [{}] {}:{} — {}\nEvidence: `{}`\nSuggestion: {}\nCriterion: {}\nTree: {}\n",
        f.finding.finding_id,
        f.finding.severity.as_str(),
        f.finding.path,
        final_line.unwrap_or(f.finding.line),
        f.finding.claim,
        f.finding.evidence,
        f.finding.suggestion.as_deref().unwrap_or("none"),
        ctx.criterion,
        f.finding.tree
    );
    if let (Some(repo), Some(base)) = (ctx.repo.as_ref(), ctx.baseline_tree.as_deref()) {
        s.push_str("```diff\n");
        s.push_str(&repo.diff(base, &f.finding.tree, 12 * 1024));
        s.push_str("\n```\n");
    }
    cap_utf8(&s, 16 * 1024)
}

struct Convening {
    subject: String,
    finding_id: Option<String>,
    trigger: CouncilTrigger,
    question: String,
    positions: Vec<CouncilPosition>,
    evidence: String,
    parties: Vec<String>,
    transcript: Vec<i64>,
    re: String,
    over_cap: bool,
}

/// `council.called`, the council (unless over the cap or out of time), then `council.ruled` —
/// every failure to rule is a `no_verdict` with its reason, which pauses.
fn convene_one(
    u: &mut UnitTeam,
    council: &dyn Council,
    clock: &Clock,
    c: Convening,
    timed_out: &mut bool,
) {
    let key = u.key();
    let mut excluded: Vec<String> = Vec::new();
    for p in &c.parties {
        if !p.is_empty() && !excluded.contains(p) {
            excluded.push(p.clone());
        }
    }
    let called = TeamEvent {
        env: env_of(&key, "engine", Some(c.re.clone())),
        body: TeamBody::CouncilCalled(CouncilCalled {
            subject: c.subject.clone(),
            finding_id: c.finding_id.clone(),
            trigger: c.trigger,
            question: c.question.clone(),
            positions: c.positions.clone(),
            evidence: c.evidence.clone(),
            excluded_seats: excluded.clone(),
            transcript: c.transcript.clone(),
        }),
    };
    u.publish(&called);
    let no = |reason: tev_reason::R| CouncilRuled {
        subject: c.subject.clone(),
        verdict: Verdict::NoVerdict,
        reason: Some(reason.0),
        task_id: None,
        consensus: false,
        agreement_pct: 0,
        dissent: Vec::new(),
        returned: 0,
        seated: 0,
    };
    let (by, ruled) = if c.over_cap {
        ("engine".to_string(), no(tev_reason::CAP))
    } else if clock.expired() {
        *timed_out = true;
        ("engine".to_string(), no(tev_reason::TIMEOUT))
    } else {
        let req = DecisionRequest {
            session_id: key.0.clone(),
            ord: key.1,
            question: c.question.clone(),
            options: c
                .positions
                .iter()
                .map(|p| format!("{}: {}", p.position, p.reason))
                .collect(),
            evidence: c.evidence.clone(),
        };
        match council.convene(req, &excluded, clock.left()) {
            CouncilOutcome::Ruled(v) => {
                let verdict = match v.winner {
                    Some(0) => Verdict::Yes,
                    Some(1) => Verdict::No,
                    _ => Verdict::NoVerdict,
                };
                (
                    format!("council:{}", v.task_id),
                    CouncilRuled {
                        subject: c.subject.clone(),
                        verdict,
                        reason: (verdict == Verdict::NoVerdict).then_some(tev_reason::NO_QUORUM.0),
                        task_id: Some(v.task_id.clone()),
                        consensus: v.consensus,
                        agreement_pct: v.agreement_pct,
                        dissent: v.dissent.clone(),
                        returned: v.returned,
                        seated: v.seated,
                    },
                )
            }
            CouncilOutcome::NoSeats(_) => ("engine".into(), no(tev_reason::SEATS_BENCHED)),
            CouncilOutcome::Failed(_) => ("engine".into(), no(tev_reason::ERROR)),
            CouncilOutcome::TimedOut => {
                *timed_out = true;
                ("engine".into(), no(tev_reason::TIMEOUT))
            }
        }
    };
    let ev = TeamEvent {
        env: env_of(&key, &by, Some(format!("council.called#{}", c.subject))),
        body: TeamBody::CouncilRuled(ruled),
    };
    u.publish(&ev);
}

/// The no-verdict reasons, spelled once.
mod tev_reason {
    use super::super::NoVerdictReason;
    pub struct R(pub NoVerdictReason);
    pub const CAP: R = R(NoVerdictReason::Cap);
    pub const TIMEOUT: R = R(NoVerdictReason::Timeout);
    pub const NO_QUORUM: R = R(NoVerdictReason::NoQuorum);
    pub const SEATS_BENCHED: R = R(NoVerdictReason::SeatsBenched);
    pub const ERROR: R = R(NoVerdictReason::Error);
}

/// The member's answer to the PA's rejection of its step: a turn on a fresh read-only session on
/// the member's seat. `None` when it cannot be asked or gives neither line (no answer on record).
fn member_answer(
    u: &UnitTeam,
    host: &dyn MonitorHost,
    member: &str,
    r: &super::StepReviewRecord,
    clock: &Clock,
) -> Option<(bool, String)> {
    if clock.expired() || host.admitted(member).is_err() {
        return None;
    }
    let key = u.key();
    let pool_key = format!("team:{}:{}:{}:step:{}", key.0, key.1, key.2, r.step_id);
    host.open(&pool_key, member, &u.scope("step")).ok()?;
    let prompt = format!(
        "[wicked-core · team member · your step was rejected]\nThe PA rejected your output for \
         step `{id}` (attempt {att}): {reason}\nIf you stand by your output, reply exactly `HOLD \
         {id} — <why it should count>`; a one-off council will decide. Otherwise reply `ACCEPT \
         {id}` and rework it. Then DONE.\n",
        id = r.step_id,
        att = r.reviewed_attempt,
        reason = r.reason
    );
    let reply = host.turn(
        &pool_key,
        &prompt,
        clock.left().min(Duration::from_secs(240)),
    );
    host.close(&pool_key);
    super::parse_member_step_answer(&reply.ok()?, &r.step_id)
}

/// S's overlays on the fold (DES-002 T1 `fold` doc): the `rejected{}` counters, corroborations
/// that arrived after a raise, final lines of findings that moved, and the members' answers to
/// rejected steps (a `false` hold is not on the stream).
fn overlay(
    l: &mut TeamLedger,
    u: &UnitTeam,
    final_lines: &BTreeMap<u32, u32>,
    step_answers: &BTreeMap<String, (bool, String)>,
) {
    l.rejected = u.book.rejected;
    for f in &mut l.findings {
        if let Some(i) = u
            .book
            .findings
            .iter()
            .position(|b| b.finding.finding_id == f.finding.finding_id)
        {
            for s in &u.book.findings[i].corroborated_by {
                if !f.corroborated_by.contains(s) {
                    f.corroborated_by.push(s.clone());
                }
            }
            if f.final_line.is_none() {
                f.final_line = final_lines.get(&u.book.raises[i]).copied();
            }
        }
    }
    for r in &mut l.step_reviews {
        if let Some((held, why)) = step_answers.get(&r.step_id) {
            if r.held.is_none() {
                r.held = Some(*held);
            }
            if r.member_reason.is_none() {
                r.member_reason = Some(why.clone());
            }
        }
    }
    l.refresh_pause();
}

// ── The thread (§4.2, §4.7) ─────────────────────────────────────────────────────────────────────

/// A handle on the running supervisor: dropping it stops the thread.
pub struct SupervisorHandle {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for SupervisorHandle {
    /// Stop the cursor thread and wait for it (in-flight final passes finish on their own
    /// threads, publishing through the same wrapper).
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// The supervisor thread's name.
pub const SUPERVISOR_THREAD: &str = "wicked-core-team-supervisor";

enum Back {
    Batch(BatchDone),
}

/// Spawn the supervisor: snapshot `T` (unless the caller already did), arm the live runs, replay
/// each from its floor up to `T`, then tail from `T`.
pub fn spawn(
    mut cfg: SupervisorConfig,
    host: Arc<dyn MonitorHost>,
    council: Arc<dyn Council>,
    live: LiveRuns,
) -> SupervisorHandle {
    let stop = Arc::new(AtomicBool::new(false));
    if cfg.tail.is_none() {
        cfg.tail = BusDb::shared(&cfg.bus_db)
            .and_then(|db| db.tail_event_id())
            .ok();
    }
    let stop_t = Arc::clone(&stop);
    let thread = match std::thread::Builder::new()
        .name(SUPERVISOR_THREAD.into())
        .spawn(move || run(cfg, host, council, live, stop_t))
    {
        Ok(t) => Some(t),
        Err(e) => {
            // Loud: without a supervisor no attempt is folded; every teamed gate waits out its
            // budget and synthesizes its fail-closed ledger (T5).
            eprintln!("wicked-core: the team supervisor did not start ({e})");
            None
        }
    };
    SupervisorHandle { stop, thread }
}

fn run(
    cfg: SupervisorConfig,
    host: Arc<dyn MonitorHost>,
    council: Arc<dyn Council>,
    live: LiveRuns,
    stop: Arc<AtomicBool>,
) {
    let (back_tx, back_rx): (Sender<Back>, Receiver<Back>) = channel();
    let poll = cfg.poll;
    let bus_db = cfg.bus_db.clone();
    // Without a tail the supervisor cannot tell history from live: it replays from 0 (every row
    // is history) and tails from what it read — a missing snapshot never skips a row.
    let tail = replay_tail(cfg.tail);
    let mut core = SupervisorCore::new(cfg, host, council);
    // §4.7 steps 2–3: arm the live runs, replay their rows up to T.
    match live() {
        Ok(runs) => {
            for r in &runs {
                core.arm(r);
            }
        }
        Err(e) => {
            eprintln!("wicked-core: team supervisor: the live runs could not be read ({e:#})")
        }
    }
    let (mut cursor, replayed) = replay(&mut core, &bus_db, tail, None);
    let spawn_job = |job: Job, core: &SupervisorCore, back: &Sender<Back>| match job {
        Job::Batch(b) => {
            let (host, back) = (core.host(), back.clone());
            let _ = std::thread::Builder::new()
                .name("wicked-core-team-batch".into())
                .spawn(move || {
                    let _ = back.send(Back::Batch(run_job(&b, &*host)));
                });
        }
        Job::FinalPass(fp) => {
            let (host, council) = (core.host(), core.council());
            let _ = std::thread::Builder::new()
                .name("wicked-core-team-final-pass".into())
                .spawn(move || run_final_pass(*fp, &*host, &*council));
        }
        Job::Help(h) => {
            let (host, bus) = (core.host(), core.publisher_bus());
            let _ = std::thread::Builder::new()
                .name("wicked-core-team-help".into())
                .spawn(move || run_help(&h, &*host, &bus));
        }
    };
    for job in replayed {
        spawn_job(job, &core, &back_tx);
    }
    while !stop.load(Ordering::SeqCst) {
        // The live tail: every row read advances the cursor, whatever run it belongs to.
        match BusDb::shared(&bus_db).and_then(|db| db.poll(TEAM_FILTER, cursor, READ_BATCH)) {
            Ok(batch) => {
                for ev in batch {
                    cursor = cursor.max(ev.event_id);
                    let Ok(event) = TeamEvent::from_payload(&ev.event_type, &ev.payload) else {
                        continue;
                    };
                    let row = TeamRow {
                        event_id: ev.event_id,
                        event,
                    };
                    for job in core.on_row(&row, false) {
                        spawn_job(job, &core, &back_tx);
                    }
                }
            }
            Err(e) => eprintln!("wicked-core: team supervisor: the bus could not be read ({e:#})"),
        }
        // Runs whose rows arrived while unarmed: ask the engine (rate-limited) and replay them.
        let due = core.unknown_due(Instant::now());
        if !due.is_empty() {
            if let Ok(runs) = live() {
                for r in runs.iter().filter(|r| due.contains(&r.run_id)) {
                    core.arm(r);
                    let jobs =
                        replay_run(&mut core, &bus_db, &r.run_id, cursor).unwrap_or_default();
                    for job in jobs {
                        spawn_job(job, &core, &back_tx);
                    }
                }
            }
        }
        core.retry_spooled();
        for job in core.due_batches(Instant::now()) {
            spawn_job(job, &core, &back_tx);
        }
        match back_rx.recv_timeout(poll) {
            Ok(Back::Batch(done)) => core.apply_batch(done),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(Back::Batch(done)) = back_rx.try_recv() {
            core.apply_batch(done);
        }
    }
}

/// The replay's upper bound from the spawn's tail snapshot.
fn replay_tail(snapshot: Option<i64>) -> i64 {
    snapshot.unwrap_or(0)
}

/// A bus read: the team rows after an event id, at most `n` of them.
type ReadRows<'a> = dyn FnMut(i64, usize) -> anyhow::Result<Vec<BusEvent>> + 'a;

/// §4.7 step 3: read every armed run's rows from the lowest floor up to `tail`, as history.
/// Returns the cursor to tail from, and the jobs a row of THIS process's own attempts made due
/// (a claim or a completion that landed before the tail was read).
fn replay(
    core: &mut SupervisorCore,
    bus_db: &str,
    tail: i64,
    only: Option<&str>,
) -> (i64, Vec<Job>) {
    let mut read =
        |after: i64, n: usize| BusDb::shared(bus_db).and_then(|db| db.poll(TEAM_FILTER, after, n));
    replay_with(core, tail, only, &mut read, READ_BATCH)
}

fn replay_with(
    core: &mut SupervisorCore,
    tail: i64,
    only: Option<&str>,
    read: &mut ReadRows<'_>,
    batch_size: usize,
) -> (i64, Vec<Job>) {
    let mut jobs = Vec::new();
    let floors = core.floors();
    let Some(from) = floors
        .iter()
        .filter(|(r, _)| only.is_none_or(|o| o == r))
        .map(|(_, f)| *f)
        .min()
    else {
        return (tail, jobs);
    };
    let mut floor = from.saturating_sub(1);
    loop {
        let batch = match read(floor, batch_size) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("wicked-core: team supervisor: replay read failed ({e:#})");
                return (tail.max(floor), jobs);
            }
        };
        let n = batch.len();
        for ev in batch {
            if ev.event_id > tail {
                return (tail, jobs);
            }
            floor = floor.max(ev.event_id);
            let Ok(event) = TeamEvent::from_payload(&ev.event_type, &ev.payload) else {
                continue;
            };
            if only.is_some_and(|o| o != event.env.run_id) {
                continue;
            }
            let row = TeamRow {
                event_id: ev.event_id,
                event,
            };
            // History starts nothing (a dead attempt is neither monitored nor folded); a row of
            // this process's own attempt is live however it was read.
            jobs.extend(core.on_row(&row, true));
        }
        if n < batch_size {
            break;
        }
    }
    for (run, st) in &core.runs {
        if st.gap {
            eprintln!(
                "wicked-core: team supervisor: stream_gap for {run}: its path.started (event {}) \
                 is no longer on the bus; nothing before the gap is carried",
                st.floor
            );
        }
    }
    (tail, jobs)
}

/// Arm-and-replay one run that surfaced live: its rows from its floor up to the cursor. Rows of
/// this process's attempts are live (their `at` is past boot), so they attach as usual.
fn replay_run(
    core: &mut SupervisorCore,
    bus_db: &str,
    run_id: &str,
    upto: i64,
) -> anyhow::Result<Vec<Job>> {
    let mut read =
        |after: i64, n: usize| BusDb::shared(bus_db).and_then(|db| db.poll(TEAM_FILTER, after, n));
    replay_run_with(core, run_id, upto, &mut read, READ_BATCH)
}

fn replay_run_with(
    core: &mut SupervisorCore,
    run_id: &str,
    upto: i64,
    read: &mut ReadRows<'_>,
    batch_size: usize,
) -> anyhow::Result<Vec<Job>> {
    let Some(floor) = core.runs.get(run_id).map(|s| s.floor) else {
        return Ok(Vec::new());
    };
    let mut jobs = Vec::new();
    let mut at = floor.saturating_sub(1);
    while let Ok(batch) = read(at, batch_size) {
        let n = batch.len();
        for ev in batch {
            if ev.event_id > upto {
                return Ok(jobs);
            }
            at = at.max(ev.event_id);
            let Ok(event) = TeamEvent::from_payload(&ev.event_type, &ev.payload) else {
                continue;
            };
            if event.env.run_id != run_id {
                continue;
            }
            jobs.extend(core.on_row(
                &TeamRow {
                    event_id: ev.event_id,
                    event,
                },
                true,
            ));
        }
        if n < batch_size {
            break;
        }
    }
    Ok(jobs)
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
pub(crate) mod tests;
