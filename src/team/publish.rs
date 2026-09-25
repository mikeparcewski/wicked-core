//! Reliable team publishing (DES-TEAMING-002 §4.1, seam P1): [`TeamBus::publish`] over the team
//! outbox, the bounded retry, the supersede rule (tombstones), per-lane FIFO, and the
//! [`TeamPublisher`] thread that publishes the engine's facts and acknowledges them to the actor.
//!
//! **One spool mechanism.** A fact the bus did not take is spooled through
//! [`wicked_apps_core::emit::spool_to`] — the same writer, the same record shape and the same loud
//! `DEADLETTER_MARKER` lines on stderr as the engine's emit outbox — into
//! `<state home>/team-outbox.ndjson`. A team line is that record plus `idempotency_key`, `run_id`,
//! `owner`, `ord` and `attempt`. Nothing here opens a second spooler.
//!
//! **Keys are recomputed, never read.** A replayed line is re-parsed with
//! [`TeamEvent::from_payload`] (which recomputes every computed field) and its key is rebuilt from
//! that; a line whose stored key disagrees is refused and counted `invalid`, never published.
//!
//! **Lanes and FIFO (§4.1 "one FIFO per publisher and run").** A lane is the owner plus the run,
//! and for the attempt runner (one runner per attempt) the `(ord, attempt)` too. A fact whose lane
//! still has an unpublished line is appended behind it and the lane drains in file order; the drain
//! stops at the first line the bus refuses. So E's `gate.decided` can never reach the bus before
//! E's `gate.opened`.
//!
//! **The supersede rule.** Two tombstone forms live only in the outbox:
//! - `{"superseded": <key>}` — that fact is never published, wherever its line sits (so a fold S
//!   spools after its deadline stays dead too, P1 (h));
//! - `{"superseded_run": <run>, "from_event": <type>, "owner": <owner|null>, "ord", "attempt"}` —
//!   every line of that run (of that owner, and that attempt when scoped) is never published,
//!   wherever it sits, with ONE exception: a `path.ended` line appended AFTER the tombstone
//!   publishes, so a rejected run still tells consumers to forget it (§4.1, row 2).
//!
//! A live [`TeamBus::publish`] obeys the same rule, so "superseded" holds for spooled and
//! not-yet-spooled facts alike. Tombstones are kept for [`TOMBSTONE_RETENTION_MS`].
//!
//! **Concurrency.** Every outbox operation — including the bus write of a drained line — runs under
//! one process-wide mutex per outbox path, so a tombstone can never land between a drain's
//! "is this line live?" check and its bus write. The actor never takes that mutex: it reaches the
//! outbox only through the [`TeamPublisher`] thread (and, at boot, before any drain is started).
//! One daemon owns a state home, so the mutex is process-local.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use anyhow::Result;
use serde_json::{json, Map, Value};

use super::events::{self, Owner, TeamEvent, PATH_ENDED};
use crate::bus::{BusDb, BusEmit};
use crate::command::Command;

/// The team outbox's file name under the daemon state home (DES-002 §4.1).
pub const TEAM_OUTBOX_FILE: &str = "team-outbox.ndjson";

/// The bounded retry schedule (§4.1): 5 attempts at 1, 2, 4, 8 and 16 s, about 31 s in all.
pub const RETRY_SCHEDULE: [Duration; 5] = [
    Duration::from_secs(1),
    Duration::from_secs(2),
    Duration::from_secs(4),
    Duration::from_secs(8),
    Duration::from_secs(16),
];

/// How long one bus write may wait for the connection and SQLite's busy handler before it counts
/// as failed. Short, because the drain holds the outbox mutex across the write.
pub const ATTEMPT_WAIT: Duration = Duration::from_millis(250);

/// How long a tombstone is kept after it is written. Past it, nothing that could still generate a
/// line for its run is alive (an attempt lasts at most 2 h, bus rows 24 h).
pub const TOMBSTONE_RETENTION_MS: i64 = 7 * 24 * 60 * 60 * 1000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// The wire token of an [`Owner`] in an outbox line.
pub fn owner_token(o: Owner) -> &'static str {
    match o {
        Owner::Engine => "engine",
        Owner::Supervisor => "supervisor",
        Owner::Runner => "runner",
    }
}

fn owner_from_token(s: &str) -> Option<Owner> {
    Some(match s {
        "engine" => Owner::Engine,
        "supervisor" => Owner::Supervisor,
        "runner" => Owner::Runner,
        _ => return None,
    })
}

// ── Configuration ────────────────────────────────────────────────────────────────────────────────

/// Where and how the engine publishes team facts. Built by [`TeamConfig::for_store`] in
/// production; tests build one with a temp outbox and a scaled schedule so they never sleep the
/// real 31 s bound and never write under a real home.
#[derive(Debug, Clone)]
pub struct TeamConfig {
    /// The bus db (`WICKED_BUS_DB`). `None` = no bus: every team run is un-teamed (§4.8 row 6).
    pub bus_db: Option<String>,
    /// The team outbox. `None` when the store has no state home (`:memory:`, `postgres://`): with
    /// nowhere to spool, a team run is un-teamed rather than published unreliably.
    pub outbox: Option<PathBuf>,
    /// The retry delays; its length is the attempt bound.
    pub schedule: Vec<Duration>,
    /// One bus write's bound ([`ATTEMPT_WAIT`]).
    pub attempt_wait: Duration,
    /// (T5) How long the worker thread waits for S's `ledger.folded` before it synthesizes the
    /// fail-closed ledger (`FINAL_PASS_BUDGET`, env-overridable; tests shorten it).
    pub final_pass_budget: Duration,
    /// (T5) The gate wait's poll interval ([`super::runner::GATE_POLL`]).
    pub gate_poll: Duration,
}

impl TeamConfig {
    /// The production config for the store at `db_path`: the bus from `WICKED_BUS_DB`, the outbox
    /// at `<state home>/team-outbox.ndjson` (the state home is the canonical parent of `--db`).
    pub fn for_store(db_path: &str) -> Self {
        Self {
            bus_db: std::env::var("WICKED_BUS_DB")
                .ok()
                .filter(|p| !p.is_empty()),
            outbox: crate::state_home::operational_home_of_db(db_path)
                .map(|home| home.join(TEAM_OUTBOX_FILE)),
            schedule: RETRY_SCHEDULE.to_vec(),
            attempt_wait: ATTEMPT_WAIT,
            // A supplied budget never lets a unit skip its gate wait: the rig override is
            // floored ([`super::runner::MIN_GATE_WAIT`]).
            final_pass_budget: crate::team::TeamLimits::from_env()
                .final_pass_budget
                .max(super::runner::MIN_GATE_WAIT),
            gate_poll: super::runner::GATE_POLL,
        }
    }

    /// An explicit config with the production schedule and budget.
    pub fn new(bus_db: Option<String>, outbox: Option<PathBuf>) -> Self {
        Self {
            bus_db,
            outbox,
            schedule: RETRY_SCHEDULE.to_vec(),
            attempt_wait: ATTEMPT_WAIT,
            final_pass_budget: crate::team::FINAL_PASS_BUDGET,
            gate_poll: super::runner::GATE_POLL,
        }
    }

    /// Replace the final-pass budget the gate wait honours (tests shorten it; T5 (c)).
    pub fn with_final_pass_budget(mut self, budget: Duration) -> Self {
        self.final_pass_budget = budget;
        self
    }

    /// Replace the gate wait's poll interval.
    pub fn with_gate_poll(mut self, poll: Duration) -> Self {
        self.gate_poll = poll;
        self
    }

    /// Replace the retry schedule (tests scale the 31 s bound down).
    pub fn with_schedule(mut self, schedule: Vec<Duration>) -> Self {
        self.schedule = schedule;
        self
    }

    /// Replace one bus write's bound.
    pub fn with_attempt_wait(mut self, wait: Duration) -> Self {
        self.attempt_wait = wait;
        self
    }

    /// The total retry bound (the sum of the schedule).
    pub fn bound(&self) -> Duration {
        self.schedule.iter().sum()
    }
}

// ── Lanes and outbox lines ───────────────────────────────────────────────────────────────────────

/// One publisher's FIFO for one run (§4.1): the owner, the run, and for the attempt runner the
/// attempt it publishes for.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Lane {
    pub owner: LaneOwner,
    pub run_id: String,
    /// `(ord, attempt)` for a runner lane; `None` for the engine and the supervisor.
    pub attempt: Option<(u32, u32)>,
}

/// [`Owner`] with an order, so lanes sort deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum LaneOwner {
    Engine,
    Supervisor,
    Runner,
}

impl From<Owner> for LaneOwner {
    fn from(o: Owner) -> Self {
        match o {
            Owner::Engine => LaneOwner::Engine,
            Owner::Supervisor => LaneOwner::Supervisor,
            Owner::Runner => LaneOwner::Runner,
        }
    }
}

impl LaneOwner {
    fn owner(self) -> Owner {
        match self {
            LaneOwner::Engine => Owner::Engine,
            LaneOwner::Supervisor => Owner::Supervisor,
            LaneOwner::Runner => Owner::Runner,
        }
    }
}

impl Lane {
    /// The lane `ev` publishes on. Errors for a runner fact without `ord`/`attempt` (its key could
    /// not be built either).
    pub fn of(ev: &TeamEvent) -> Result<Lane> {
        let owner = events::owner(ev.event_type())
            .ok_or_else(|| anyhow::anyhow!("`{}` has no team owner", ev.event_type()))?;
        let attempt = match owner {
            Owner::Runner => match (ev.env.ord, ev.env.attempt) {
                (Some(o), Some(a)) => Some((o, a)),
                _ => anyhow::bail!("runner fact `{}` needs ord and attempt", ev.event_type()),
            },
            _ => None,
        };
        Ok(Lane {
            owner: owner.into(),
            run_id: ev.env.run_id.clone(),
            attempt,
        })
    }
}

/// A fact line: the event, re-parsed, with its recomputed key and lane.
#[derive(Debug, Clone)]
struct Fact {
    lane: Lane,
    key: String,
    event_type: String,
    emit: BusEmit,
}

impl Fact {
    fn of(ev: &TeamEvent) -> Result<Fact> {
        Ok(Fact {
            lane: Lane::of(ev)?,
            key: ev.key()?,
            event_type: ev.event_type().to_string(),
            emit: ev.bus_emit()?,
        })
    }
}

/// A run tombstone's scope.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RunTombstone {
    run_id: String,
    owner: Option<LaneOwner>,
    attempt: Option<(u32, u32)>,
}

impl RunTombstone {
    fn covers(&self, lane: &Lane) -> bool {
        self.run_id == lane.run_id
            && self.owner.is_none_or(|o| o == lane.owner)
            && self.attempt.is_none_or(|a| Some(a) == lane.attempt)
    }
}

#[derive(Debug, Clone)]
enum Line {
    Fact(Fact),
    Superseded {
        key: String,
        ts: i64,
    },
    SupersededRun {
        scope: RunTombstone,
        ts: i64,
    },
    /// Torn, unparseable, or a key that does not match its payload: kept in the file for the
    /// operator, never published, never blocking a lane.
    Invalid(String),
}

/// The string value after `"<field>":"` in a line that is not valid JSON (a torn append), if the
/// value is complete.
fn torn_field(raw: &str, field: &str) -> Option<String> {
    let at = raw.find(&format!("\"{field}\":\""))? + field.len() + 4;
    let end = raw[at..].find('"')?;
    Some(raw[at..at + end].to_string()).filter(|v| !v.is_empty())
}

fn parse_line(raw: &str) -> Line {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            // A torn line (a crash mid-append) that names a tombstone still supersedes what it
            // names — fail closed: a tombstone is never lost to a partial write. Its `ts` is
            // unknown, so it never ages out.
            if let Some(run) = torn_field(raw, "superseded_run") {
                return Line::SupersededRun {
                    scope: RunTombstone {
                        run_id: run,
                        owner: None,
                        attempt: None,
                    },
                    ts: i64::MAX,
                };
            }
            if let Some(key) = torn_field(raw, "superseded") {
                return Line::Superseded { key, ts: i64::MAX };
            }
            return Line::Invalid(format!("not JSON: {e}"));
        }
    };
    let ts = v.get("ts").and_then(Value::as_i64).unwrap_or(0);
    if let Some(key) = v.get("superseded").and_then(Value::as_str) {
        return Line::Superseded {
            key: key.to_string(),
            ts,
        };
    }
    if let Some(run) = v.get("superseded_run").and_then(Value::as_str) {
        let owner = match v.get("owner") {
            None | Some(Value::Null) => None,
            // An owner this build cannot read covers EVERY owner (the broader tombstone).
            Some(o) => o.as_str().and_then(owner_from_token).map(LaneOwner::from),
        };
        let attempt = match (
            v.get("ord").and_then(Value::as_u64),
            v.get("attempt").and_then(Value::as_u64),
        ) {
            (Some(o), Some(a)) => Some((o as u32, a as u32)),
            _ => None,
        };
        return Line::SupersededRun {
            scope: RunTombstone {
                run_id: run.to_string(),
                owner,
                attempt,
            },
            ts,
        };
    }
    let (Some(event_type), Some(payload), Some(stored_key)) = (
        v.get("type").and_then(Value::as_str),
        v.get("payload"),
        v.get("idempotency_key").and_then(Value::as_str),
    ) else {
        return Line::Invalid("not a team fact or tombstone".into());
    };
    // Computed fields are never read from input: re-parse, then rebuild the key from the event.
    let ev = match TeamEvent::from_payload(event_type, payload) {
        Ok(ev) => ev,
        Err(e) => return Line::Invalid(format!("`{event_type}` payload refused: {e}")),
    };
    match Fact::of(&ev) {
        Ok(f) if f.key == stored_key => Line::Fact(f),
        Ok(f) => Line::Invalid(format!(
            "stored key {stored_key} is not the event's key {}",
            f.key
        )),
        Err(e) => Line::Invalid(format!("`{event_type}`: {e}")),
    }
}

/// The parsed outbox, in file order.
struct Snapshot {
    lines: Vec<(String, Line)>,
}

impl Snapshot {
    /// Whether `fact` at file index `at` (`None` = not in the file yet: a live publish) is
    /// superseded.
    fn superseded(&self, fact: &Fact, at: Option<usize>) -> bool {
        self.lines.iter().enumerate().any(|(j, (_, l))| match l {
            Line::Superseded { key, .. } => *key == fact.key,
            Line::SupersededRun { scope, .. } => {
                // A `path.ended` appended after the tombstone is the one fact that still goes.
                let ended_after = fact.event_type == PATH_ENDED && at.is_none_or(|i| i > j);
                scope.covers(&fact.lane) && !ended_after
            }
            _ => false,
        })
    }

    /// The live (unpublished, unsuperseded) facts of `lane`, `(file index, fact)`, in order.
    fn pending(&self, lane: &Lane) -> Vec<(usize, Fact)> {
        self.lines
            .iter()
            .enumerate()
            .filter_map(|(i, (_, l))| match l {
                Line::Fact(f) if f.lane == *lane && !self.superseded(f, Some(i)) => {
                    Some((i, f.clone()))
                }
                _ => None,
            })
            .collect()
    }

    fn lanes(&self) -> Vec<Lane> {
        let mut seen = Vec::new();
        for (_, l) in &self.lines {
            if let Line::Fact(f) = l {
                if !seen.contains(&f.lane) {
                    seen.push(f.lane.clone());
                }
            }
        }
        seen
    }
}

// ── The outbox file ──────────────────────────────────────────────────────────────────────────────

fn guard(m: &Mutex<()>) -> MutexGuard<'_, ()> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn outbox_mutex(path: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    map.entry(path.to_path_buf()).or_default().clone()
}

/// What one drain did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct DrainReport {
    /// `(idempotency key, bus event id)` of every fact that reached the bus, in publish order.
    pub published: Vec<(String, i64)>,
    /// Facts skipped (and compacted away) because a tombstone superseded them.
    pub superseded: usize,
    /// Lines refused as torn or mis-keyed (kept in the file, never published).
    pub invalid: usize,
    /// Facts still in the outbox after the drain.
    pub remaining: usize,
    /// Why the first refused lane stopped, per lane that stopped (`run_id`, reason).
    pub failures: Vec<(String, String)>,
}

/// What [`TeamBus::publish`] came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublishOutcome {
    /// On the bus, at this `event_id` (a duplicate key resolves to the existing row).
    Published(i64),
    /// Not on the bus: the line is in the outbox (spooled now, or already there), with why.
    Spooled(String),
    /// A tombstone supersedes the fact: it is not published and not spooled.
    Superseded,
}

/// The one publish wrapper every team publisher calls (§4.1): the engine through the
/// [`TeamPublisher`], the supervisor and the attempt runner on their own threads.
#[derive(Debug, Clone)]
pub struct TeamBus {
    bus_db: String,
    outbox: PathBuf,
    attempt_wait: Duration,
}

impl TeamBus {
    pub fn new(
        bus_db: impl Into<String>,
        outbox: impl Into<PathBuf>,
        attempt_wait: Duration,
    ) -> Self {
        Self {
            bus_db: bus_db.into(),
            outbox: outbox.into(),
            attempt_wait,
        }
    }

    pub fn outbox_path(&self) -> &Path {
        &self.outbox
    }

    fn mutex(&self) -> Arc<Mutex<()>> {
        outbox_mutex(&self.outbox)
    }

    /// The outbox, parsed. A missing file is an empty outbox; any OTHER read failure is an error —
    /// an unreadable outbox may hold tombstones, so it is never read as empty (fail closed).
    fn read(&self) -> std::io::Result<Snapshot> {
        let text = match std::fs::read_to_string(&self.outbox) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        Ok(Snapshot {
            lines: text
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(|l| (l.to_string(), parse_line(l)))
                .collect(),
        })
    }

    /// One bounded bus write.
    fn emit(&self, e: &BusEmit) -> Result<i64> {
        let db = BusDb::shared(&self.bus_db)?;
        match db.emit_bounded(e, self.attempt_wait)? {
            Some(id) => Ok(id),
            None => anyhow::bail!("bus connection busy in this process"),
        }
    }

    fn spool(&self, fact: &Fact, reason: &str) -> std::io::Result<()> {
        let event = wicked_apps_core::emit::EmitEvent::new(
            fact.emit.event_type.clone(),
            fact.emit.domain.clone(),
            fact.emit.subdomain.clone(),
            fact.emit.payload.clone(),
        );
        let mut extra = Map::new();
        extra.insert("idempotency_key".into(), json!(fact.key));
        extra.insert("run_id".into(), json!(fact.lane.run_id));
        extra.insert("owner".into(), json!(owner_token(fact.lane.owner.owner())));
        extra.insert("ord".into(), json!(fact.lane.attempt.map(|a| a.0)));
        extra.insert("attempt".into(), json!(fact.lane.attempt.map(|a| a.1)));
        wicked_apps_core::emit::spool_to(&self.outbox, &event, reason, extra)
    }

    /// Publish `ev` reliably: straight to the bus when its lane has nothing waiting, else behind
    /// the lane's earlier lines (FIFO); a refused write is spooled with a `DEADLETTER_MARKER`. A
    /// superseded fact is neither published nor spooled. `Err` only when the event cannot be keyed
    /// or its line cannot be written.
    pub fn publish(&self, ev: &TeamEvent) -> Result<PublishOutcome> {
        let fact = Fact::of(ev)?;
        let m = self.mutex();
        let _g = guard(&m);
        let snap = self
            .read()
            .map_err(|e| anyhow::anyhow!("the team outbox cannot be read ({e}); not publishing"))?;
        if snap.superseded(&fact, None) {
            return Ok(PublishOutcome::Superseded);
        }
        let pending = snap.pending(&fact.lane);
        if pending.is_empty() {
            return match self.emit(&fact.emit) {
                Ok(id) => Ok(PublishOutcome::Published(id)),
                Err(e) => {
                    let reason = format!("bus write failed: {e:#}");
                    self.spool(&fact, &reason)?;
                    Ok(PublishOutcome::Spooled(reason))
                }
            };
        }
        if !pending.iter().any(|(_, f)| f.key == fact.key) {
            self.spool(
                &fact,
                "queued behind an unpublished fact of the same publisher and run (FIFO)",
            )?;
        }
        let report = self.drain_locked(Some(&fact.lane));
        Ok(outcome_for(&report, &fact.key))
    }

    /// Drain one lane in order (the live drain). Stops at the first refused line.
    pub fn drain_lane(&self, lane: &Lane) -> DrainReport {
        let m = self.mutex();
        let _g = guard(&m);
        self.drain_locked(Some(lane))
    }

    /// Drain only the lanes of `runs` — the boot drain, limited to the runs the boot reconcile
    /// read and reconciled: a line of a run it could not read stays for an explicit replay.
    pub fn drain_runs(&self, runs: &[String]) -> DrainReport {
        let m = self.mutex();
        let _g = guard(&m);
        self.drain_filtered(|lane| runs.contains(&lane.run_id))
    }

    /// Spool `ev` into the outbox WITHOUT trying the bus — for a fact the engine must not lose
    /// while it has no publisher (a terminal run's `path.ended` on a daemon with no bus). Obeys
    /// the supersede rule; a fact already waiting is not spooled twice.
    pub(crate) fn spool_only(&self, ev: &TeamEvent, reason: &str) -> Result<()> {
        let fact = Fact::of(ev)?;
        let m = self.mutex();
        let _g = guard(&m);
        let snap = self.read()?;
        if snap.superseded(&fact, None)
            || snap
                .pending(&fact.lane)
                .iter()
                .any(|(_, f)| f.key == fact.key)
        {
            return Ok(());
        }
        self.spool(&fact, reason)?;
        Ok(())
    }

    /// Drain every lane in order — [`Core::replay_team_outbox`](crate::Core::replay_team_outbox).
    /// Idempotent: a line replayed twice lands once (the key resolves to the existing row).
    pub fn drain_all(&self) -> DrainReport {
        let m = self.mutex();
        let _g = guard(&m);
        self.drain_locked(None)
    }

    fn drain_locked(&self, only: Option<&Lane>) -> DrainReport {
        self.drain_filtered(|lane| only.is_none_or(|l| l == lane))
    }

    /// Drain the lanes `keep` selects, in order. An unreadable outbox drains nothing.
    fn drain_filtered(&self, keep: impl Fn(&Lane) -> bool) -> DrainReport {
        let mut report = DrainReport::default();
        let snap = match self.read() {
            Ok(s) => s,
            Err(e) => {
                report
                    .failures
                    .push(("*".into(), format!("the team outbox cannot be read: {e}")));
                return report;
            }
        };
        let lanes: Vec<Lane> = snap.lanes().into_iter().filter(|l| keep(l)).collect();
        let mut published_ix: Vec<usize> = Vec::new();
        for lane in lanes {
            for (ix, fact) in snap.pending(&lane) {
                match self.emit(&fact.emit) {
                    Ok(id) => {
                        report.published.push((fact.key.clone(), id));
                        published_ix.push(ix);
                    }
                    Err(e) => {
                        report
                            .failures
                            .push((lane.run_id.clone(), format!("bus write failed: {e:#}")));
                        break;
                    }
                }
            }
        }
        self.compact_locked(&snap, &published_ix, &mut report);
        report
    }

    /// Rewrite the outbox without the published lines, the superseded facts and the expired
    /// tombstones (write-then-rename, so a crash leaves the old file or the new one).
    fn compact_locked(&self, snap: &Snapshot, published: &[usize], report: &mut DrainReport) {
        let now = now_ms();
        let mut keep: Vec<&str> = Vec::new();
        let mut dropped = false;
        for (i, (raw, line)) in snap.lines.iter().enumerate() {
            let drop = match line {
                Line::Fact(f) => {
                    if published.contains(&i) {
                        true
                    } else if snap.superseded(f, Some(i)) {
                        report.superseded += 1;
                        true
                    } else {
                        report.remaining += 1;
                        false
                    }
                }
                Line::Superseded { ts, .. } | Line::SupersededRun { ts, .. } => {
                    now.saturating_sub(*ts) > TOMBSTONE_RETENTION_MS
                }
                Line::Invalid(why) => {
                    report.invalid += 1;
                    eprintln!(
                        "{} team outbox line kept, not published ({why})",
                        wicked_apps_core::emit::DEADLETTER_MARKER
                    );
                    false
                }
            };
            dropped |= drop;
            if !drop {
                keep.push(raw);
            }
        }
        if !dropped {
            return;
        }
        let tmp = self.outbox.with_extension("ndjson.compact");
        let mut body = keep.join("\n");
        if !body.is_empty() {
            body.push('\n');
        }
        let res = std::fs::write(&tmp, body).and_then(|_| std::fs::rename(&tmp, &self.outbox));
        if let Err(e) = res {
            eprintln!(
                "wicked-core: team outbox compaction failed ({e}); published lines stay and \
                 dedup by key on the next drain"
            );
        }
    }

    fn append(&self, record: Value) -> std::io::Result<()> {
        wicked_apps_core::emit::append_record(&self.outbox, &record)
    }

    /// Supersede one fact by key (§4.1 tombstone form 1).
    pub fn supersede_fact(&self, key: &str, reason: &str) -> std::io::Result<()> {
        let m = self.mutex();
        let _g = guard(&m);
        self.append(json!({
            "superseded": key, "reason": reason, "ts": now_ms(), "pid": std::process::id(),
        }))
    }

    /// Supersede every line of `run_id` (of `owner`, and of `attempt` when given; every owner
    /// when `owner` is `None`) (§4.1 tombstone form 2). `from_event` names the fact whose failure
    /// triggered the fallback.
    pub fn supersede_run(
        &self,
        run_id: &str,
        owner: Option<Owner>,
        attempt: Option<(u32, u32)>,
        from_event: &str,
        reason: &str,
    ) -> std::io::Result<()> {
        let m = self.mutex();
        let _g = guard(&m);
        self.append(json!({
            "superseded_run": run_id,
            "from_event": from_event,
            "owner": owner.map(owner_token),
            "ord": attempt.map(|a| a.0),
            "attempt": attempt.map(|a| a.1),
            "reason": reason,
            "ts": now_ms(),
            "pid": std::process::id(),
        }))
    }

    /// Whether the fact keyed `key` is still waiting in the outbox (unpublished, unsuperseded).
    pub fn is_pending(&self, key: &str) -> bool {
        let m = self.mutex();
        let _g = guard(&m);
        // Unreadable: assume it is still there (never report a fact gone that may not be).
        let Ok(snap) = self.read() else {
            return true;
        };
        snap.lines.iter().enumerate().any(|(i, (_, l))| {
            matches!(l, Line::Fact(f) if f.key == key && !snap.superseded(f, Some(i)))
        })
    }
}

fn outcome_for(report: &DrainReport, key: &str) -> PublishOutcome {
    match report.published.iter().find(|(k, _)| k == key) {
        Some((_, id)) => PublishOutcome::Published(*id),
        None => PublishOutcome::Spooled(
            report
                .failures
                .first()
                .map(|(_, r)| r.clone())
                .unwrap_or_else(|| "waiting behind an earlier fact".into()),
        ),
    }
}

/// Supersede a run's lines directly in the outbox at `outbox` — for the actor's BOOT reconcile
/// only (§4.1 "crash between tombstone and store write"), which runs before any drain starts.
pub(crate) fn supersede_run_at(
    outbox: &Path,
    run_id: &str,
    from_event: &str,
    reason: &str,
) -> std::io::Result<()> {
    TeamBus::new("", outbox, ATTEMPT_WAIT).supersede_run(run_id, None, None, from_event, reason)
}

// ── The engine's publisher thread (§4.1 "the TeamPublisher") ─────────────────────────────────────

/// Identifies one engine fact the actor is waiting on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamToken {
    pub run_id: String,
    pub event_type: String,
    pub key: String,
}

/// What the publisher does when a fact's bound runs out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exhausted {
    /// Leave the line for `replay_team_outbox` (a pause is reversible).
    Keep,
    /// The fallback is irreversible (`path.started` → `transport:"none"`): tombstone the run's
    /// lines BEFORE acknowledging the failure (§4.1).
    SupersedeRun,
}

/// A request to the publisher thread.
#[derive(Debug)]
pub enum PublisherReq {
    /// Publish `event`. With `ack`, the actor waits for `TeamPublished` / `TeamTransportFailed`.
    Publish {
        event: Box<TeamEvent>,
        ack: Option<(TeamToken, Exhausted)>,
    },
    /// Write a run tombstone, then acknowledge with `TeamSuperseded` (when `ack`).
    SupersedeRun {
        run_id: String,
        from_event: String,
        reason: String,
        ack: Option<TeamToken>,
    },
    /// Drain the lanes of these runs once (the actor's boot drain: only runs it reconciled).
    DrainRuns(Vec<String>),
}

/// The actor's link to team publishing.
#[derive(Debug, Clone)]
pub enum TeamLink {
    /// A publisher thread and the outbox it owns, plus the attempt runner's handle on the same
    /// bus and outbox (T5), which the actor hands to every worker thread it starts.
    Publisher {
        tx: Sender<PublisherReq>,
        outbox: PathBuf,
        runner: Box<super::runner::TeamRunner>,
    },
    /// No publisher (no bus, or it did not start): every NEW team run is un-teamed, with this
    /// reason. The outbox is the state home's whatever the bus: a run tombstone the engine owes
    /// (a finished answer, an ended run) is still written there.
    Unavailable {
        reason: String,
        outbox: Option<PathBuf>,
    },
}

impl TeamLink {
    /// Spawn the engine's publisher for `cfg`, acknowledging to the actor through `self_tx`.
    pub(crate) fn spawn(cfg: &TeamConfig, self_tx: Sender<Command>) -> TeamLink {
        let (Some(bus_db), Some(outbox)) = (cfg.bus_db.clone(), cfg.outbox.clone()) else {
            return TeamLink::Unavailable {
                reason: if cfg.bus_db.is_none() {
                    "no bus (WICKED_BUS_DB unset)".into()
                } else {
                    "no state home for the team outbox".into()
                },
                outbox: cfg.outbox.clone(),
            };
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let bus = TeamBus::new(bus_db, outbox.clone(), cfg.attempt_wait);
        let schedule = if cfg.schedule.is_empty() {
            RETRY_SCHEDULE.to_vec()
        } else {
            cfg.schedule.clone()
        };
        let spawned = std::thread::Builder::new()
            .name(TEAM_PUBLISHER_THREAD.into())
            .spawn(move || publisher_loop(bus, schedule, rx, self_tx));
        let Some(runner) = super::runner::TeamRunner::from_config(cfg) else {
            return TeamLink::Unavailable {
                reason: "no team runner for this config".into(),
                outbox: cfg.outbox.clone(),
            };
        };
        match spawned {
            Ok(_) => TeamLink::Publisher {
                tx,
                outbox,
                runner: Box::new(runner),
            },
            Err(e) => TeamLink::Unavailable {
                reason: format!("team publisher did not start: {e}"),
                outbox: cfg.outbox.clone(),
            },
        }
    }
}

/// The publisher thread's name.
pub const TEAM_PUBLISHER_THREAD: &str = "wicked-core-team-publisher";

struct Waiter {
    event: Box<TeamEvent>,
    run_id: String,
    ack: Option<(TeamToken, Exhausted)>,
    /// Retries done so far.
    tries: usize,
    due: Instant,
}

fn publisher_loop(
    bus: TeamBus,
    schedule: Vec<Duration>,
    rx: Receiver<PublisherReq>,
    self_tx: Sender<Command>,
) {
    let mut waiting: Vec<Waiter> = Vec::new();
    loop {
        let timeout = waiting
            .iter()
            .map(|w| w.due.saturating_duration_since(Instant::now()))
            .min()
            .unwrap_or(Duration::from_secs(3600));
        match rx.recv_timeout(timeout) {
            Ok(req) => handle_req(&bus, &schedule, &self_tx, &mut waiting, req),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        retry_due(&bus, &schedule, &self_tx, &mut waiting);
    }
}

fn ack_published(self_tx: &Sender<Command>, ack: Option<(TeamToken, Exhausted)>, id: i64) {
    if let Some((token, _)) = ack {
        let _ = self_tx.send(Command::TeamPublished {
            token,
            event_id: id,
        });
    }
}

fn ack_failed(self_tx: &Sender<Command>, ack: Option<(TeamToken, Exhausted)>, reason: String) {
    if let Some((token, _)) = ack {
        let _ = self_tx.send(Command::TeamTransportFailed { token, reason });
    }
}

fn handle_req(
    bus: &TeamBus,
    schedule: &[Duration],
    self_tx: &Sender<Command>,
    waiting: &mut Vec<Waiter>,
    req: PublisherReq,
) {
    match req {
        PublisherReq::Publish { event, ack } => match bus.publish(&event) {
            Ok(PublishOutcome::Published(id)) => ack_published(self_tx, ack, id),
            Ok(PublishOutcome::Superseded) => ack_failed(
                self_tx,
                ack,
                "superseded: the run moved past this fact".into(),
            ),
            Ok(PublishOutcome::Spooled(_)) => waiting.push(Waiter {
                run_id: event.env.run_id.clone(),
                event,
                ack,
                tries: 0,
                due: Instant::now() + schedule[0],
            }),
            Err(e) => ack_failed(
                self_tx,
                ack,
                format!("the fact could not be keyed or spooled: {e:#}"),
            ),
        },
        PublisherReq::SupersedeRun {
            run_id,
            from_event,
            reason,
            ack,
        } => {
            if let Err(e) = bus.supersede_run(&run_id, None, None, &from_event, &reason) {
                // Fail closed: the actor keeps the run paused rather than persisting a fallback
                // the outbox does not agree with.
                ack_failed(
                    self_tx,
                    ack.map(|t| (t, Exhausted::Keep)),
                    format!("the run tombstone could not be written: {e}"),
                );
                return;
            }
            // The run's engine waiters are moot now; their facts are superseded.
            waiting.retain(|w| w.run_id != run_id);
            if let Some(token) = ack {
                let _ = self_tx.send(Command::TeamSuperseded { token });
            }
        }
        PublisherReq::DrainRuns(runs) => {
            let r = bus.drain_runs(&runs);
            if !r.published.is_empty() || r.remaining > 0 {
                eprintln!(
                    "wicked-core: team outbox drained at boot: {} published, {} superseded, {} \
                     remaining",
                    r.published.len(),
                    r.superseded,
                    r.remaining
                );
            }
        }
    }
}

/// Retry every due waiter by publishing its fact again: [`TeamBus::publish`] drains the fact's
/// lane in order first (FIFO), resolves a fact another drain already published to its existing
/// row (the key), and refuses a superseded one. Past the bound a waiter is acknowledged as failed
/// (for an irreversible fallback, only after the run tombstone is written) and its line stays for
/// `replay_team_outbox`.
fn retry_due(
    bus: &TeamBus,
    schedule: &[Duration],
    self_tx: &Sender<Command>,
    waiting: &mut Vec<Waiter>,
) {
    let now = Instant::now();
    let mut keep = Vec::new();
    for mut w in std::mem::take(waiting) {
        if w.due > now {
            keep.push(w);
            continue;
        }
        let why = match bus.publish(&w.event) {
            Ok(PublishOutcome::Published(id)) => {
                ack_published(self_tx, w.ack, id);
                continue;
            }
            Ok(PublishOutcome::Superseded) => {
                ack_failed(
                    self_tx,
                    w.ack,
                    "superseded: the run moved past this fact".into(),
                );
                continue;
            }
            Ok(PublishOutcome::Spooled(why)) => why,
            Err(e) => format!("{e:#}"),
        };
        w.tries += 1;
        if w.tries < schedule.len() {
            w.due = now + schedule[w.tries];
            keep.push(w);
            continue;
        }
        let bound: Duration = schedule.iter().sum();
        let Some((token, exhausted)) = w.ack.take() else {
            // Nobody waits on it: the line stays in the outbox for `replay_team_outbox`.
            continue;
        };
        let reason = format!(
            "{} could not be published: the bus refused it for {} attempts over {bound:.0?} \
             (last: {why})",
            token.event_type,
            schedule.len()
        );
        if exhausted == Exhausted::SupersedeRun {
            // Tombstone BEFORE the fallback is acknowledged (§4.1).
            if let Err(e) = bus.supersede_run(&token.run_id, None, None, &token.event_type, &reason)
            {
                eprintln!(
                    "wicked-core: team tombstone for {} not written ({e}); retrying",
                    token.run_id
                );
                w.ack = Some((token, exhausted));
                w.due = now + *schedule.last().unwrap_or(&Duration::from_secs(1));
                keep.push(w);
                continue;
            }
        }
        let _ = self_tx.send(Command::TeamTransportFailed { token, reason });
    }
    keep.append(waiting);
    *waiting = keep;
}

// ── Reads (the core half of crew's `GET /api/v1/runs/:id/team`) ──────────────────────────────────

/// A run's team state as the read route serves it: `transport` is `bus`, `none`, or `pending`
/// (not decided yet — the run dispatches nothing while pending).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunTeamView {
    pub run_id: String,
    pub transport: String,
    pub reason: Option<String>,
    pub stream_floor: Option<i64>,
    pub plan_rev: Option<u32>,
    /// The required fact the run waits on, if any (its event type).
    pub pending: Option<String>,
    /// The run is terminal and its end is on record: `path.ended` acknowledged, or the run
    /// tombstone written for a run that never published its path.
    pub ended: bool,
    /// Every unit's team snapshot, in plan order (`None` transport = not dispatched yet).
    pub units: Vec<UnitTeamView>,
}

/// One unit's team snapshot in a [`RunTeamView`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnitTeamView {
    pub ord: u32,
    pub transport: Option<String>,
    pub reason: Option<String>,
    pub ledger_source: Option<String>,
    /// (T5) The S row the gate read; `None` for a synthesized or local snapshot.
    pub ledger_ref: Option<String>,
    /// (T5) The attempt ledger's `final_pass`, once the unit folded.
    pub final_pass: Option<String>,
    /// (T5) Whether the attempt's ledger pauses the run (DES-001 §6.7).
    pub team_pause: Option<bool>,
    /// (T5) How many findings the attempt's ledger holds.
    pub findings: Option<usize>,
}

/// A live teamed run and its team state (§4.7 replay step 2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveTeamRun {
    pub run_id: String,
    pub status: crate::domain::SessionStatus,
    pub team: crate::domain::RunTeamState,
}

/// Build the read view for a team run (`None` for a run that is not a team run).
pub(crate) fn run_team_view(
    session: &crate::domain::AgentSession,
    units: &[crate::domain::WorkUnit],
    has_publisher: bool,
) -> Option<RunTeamView> {
    if session.team.is_none() && !units.iter().any(|u| u.team_run) {
        return None;
    }
    let team = session.team.clone().unwrap_or_default();
    // A run is reported teamed only while THIS process can publish its facts: with no publisher
    // (bus present at launch, absent now) a teamed run is `unavailable`, never live-teamed.
    let transport = match team.transport {
        Some(events::Transport::None) => "none".to_string(),
        Some(_) if team.is_teamed() && has_publisher => "bus".to_string(),
        Some(_) if team.is_teamed() => "unavailable".to_string(),
        _ => "pending".to_string(),
    };
    Some(RunTeamView {
        run_id: session.id.clone(),
        transport,
        reason: team.reason.clone(),
        stream_floor: team.stream_floor,
        plan_rev: team.plan_rev,
        pending: team.pending.as_ref().map(|p| p.event_type.clone()),
        ended: team.ended,
        units: units
            .iter()
            .filter(|u| u.team_run)
            .map(|u| UnitTeamView {
                ord: u.ord,
                transport: u.team.as_ref().map(|t| t.transport.as_str().to_string()),
                reason: u.team.as_ref().and_then(|t| t.reason.clone()),
                ledger_source: u
                    .team
                    .as_ref()
                    .and_then(|t| t.ledger_source)
                    .map(|l| l.as_str().to_string()),
                ledger_ref: u.team.as_ref().and_then(|t| t.ledger_ref.clone()),
                final_pass: u
                    .team
                    .as_ref()
                    .and_then(|t| t.ledger.as_ref())
                    .map(|l| l.final_pass.as_str().to_string()),
                team_pause: u
                    .team
                    .as_ref()
                    .and_then(|t| t.ledger.as_ref())
                    .map(|l| l.team_pause),
                findings: u
                    .team
                    .as_ref()
                    .and_then(|t| t.ledger.as_ref())
                    .map(|l| l.findings.len()),
            })
            .collect(),
    })
}

#[cfg(test)]
#[path = "publish_tests.rs"]
pub(crate) mod tests;
