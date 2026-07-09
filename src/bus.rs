//! The Rust↔wicked-bus bridge (DES-EXEC-001 §2.5) — the seam that makes launch-as-event + lifecycle
//! events REAL across the language boundary. wicked-bus is a local-first SQLite event log (a Node
//! package) living in its OWN database file, separate from the estate store the actor owns. This
//! module gives core three capabilities against that log, all speaking the *same* SQLite schema the
//! JS bus reads/writes so a row written here is indistinguishable from one the JS `emit()` wrote:
//!
//!  1. [`BusDb::emit`] — publish a `wicked.<noun>.<verb>` event (deterministic idempotency key, the
//!     bus's two-timer TTL) into the `events` table.
//!  2. [`BusDb::poll`] — read events by filter past an integer cursor floor (at-least-once, idempotent
//!     — the caller advances its own floor; a re-read is harmless because the launch it drives is
//!     idempotent by run id).
//!  3. [`spawn_run_requested_poller`] / [`connect`] — a DEDICATED thread that turns each
//!     `wicked.run.requested {workflow, problem, args}` into a `Command::LaunchRun` posted to the
//!     actor, and (proof of the publish path) emits `wicked.run.launched` back onto the bus.
//!
//! ## Why raw SQL and not `wicked_apps_core::emit_event_to`
//! `emit_event_to` writes an `EVENT` *node* onto the ESTATE graph store — a different database with a
//! different (graph-node) schema. The wicked-bus `events` table is a distinct relational log. §2.5's
//! "same events table" is the wicked-bus table, so the bridge writes it directly. The row shape is
//! taken from wicked-bus `lib/schema.sql` + migration 3 (the added causality/registry columns are all
//! nullable, so writing the base NOT NULL columns is sufficient and forward-compatible).
//!
//! ## Actor-safety (the load-bearing invariant)
//! The poll loop NEVER runs on the single-writer actor thread. [`spawn_run_requested_poller`] runs on
//! its own `std::thread`, opens its OWN `rusqlite` connection to the bus db (a different file from the
//! estate store — no writer-lock contention with the actor), and reaches the actor ONLY by sending
//! `Command::LaunchRun` over a `Sender<Command>` clone. That is exactly the `self_tx` write-back
//! pattern the unit workers already use: a blocking SQLite poll on the bus db can never stall the
//! actor, and the launch itself is applied serially on the actor like any other command.
//!
//! ## Cross-platform
//! `rusqlite` is built with the `bundled` feature (SQLite compiled from source — no system libsqlite
//! dependency), timestamps are plain `SystemTime` epoch-millis, and paths are caller-supplied — no
//! unix-only APIs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use wicked_council::AgenticCli;

use crate::command::Command;
use crate::scope::EntityMode;
use crate::{HumanConfirm, LaunchSpec};

/// wicked-bus config default: an event is pollable for 72h (`config.ttl_hours`).
pub const DEFAULT_TTL_HOURS: i64 = 72;
/// wicked-bus config default: the idempotency dedup row survives 24h (`config.dedup_ttl_hours`).
pub const DEFAULT_DEDUP_TTL_HOURS: i64 = 24;
const MS_PER_HOUR: i64 = 3_600_000;

/// The base `events` table DDL — byte-for-byte the NOT NULL/UNIQUE shape from wicked-bus
/// `lib/schema.sql` (schema versions 1+2). `CREATE ... IF NOT EXISTS`, so it is a no-op when the JS
/// bus already created the file, and JS `openDb()` is equally a no-op (plus its migration 3, which
/// only *adds nullable columns*) when Rust created it first. The seeded `schema_migrations` rows keep
/// the JS `checkSchemaVersion` happy and let JS `migrate()` layer its v3 columns on top.
const EVENTS_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS events (
    event_id          INTEGER PRIMARY KEY AUTOINCREMENT,
    event_type        TEXT    NOT NULL CHECK(length(event_type) <= 128),
    domain            TEXT    NOT NULL CHECK(length(domain) <= 64),
    subdomain         TEXT    NOT NULL DEFAULT '' CHECK(length(subdomain) <= 64),
    payload           TEXT    NOT NULL,
    schema_version    TEXT    NOT NULL DEFAULT '1.0.0',
    idempotency_key   TEXT    NOT NULL UNIQUE,
    emitted_at        INTEGER NOT NULL,
    expires_at        INTEGER NOT NULL,
    dedup_expires_at  INTEGER NOT NULL,
    metadata          TEXT
);
CREATE INDEX IF NOT EXISTS idx_events_event_type ON events(event_type);
CREATE INDEX IF NOT EXISTS idx_events_domain     ON events(domain);
CREATE INDEX IF NOT EXISTS idx_events_expires_at ON events(expires_at);
CREATE TABLE IF NOT EXISTS schema_migrations (
    version     INTEGER PRIMARY KEY,
    applied_at  INTEGER NOT NULL,
    description TEXT
);
INSERT OR IGNORE INTO schema_migrations(version, applied_at, description)
VALUES (1, (strftime('%s','now') * 1000), 'initial schema');
INSERT OR IGNORE INTO schema_migrations(version, applied_at, description)
VALUES (2, (strftime('%s','now') * 1000), 'add dead_letters and delivery_attempts tables');
"#;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A `wicked.<noun>.<verb>` event ready to publish onto the bus. `domain` is the publisher identity
/// (e.g. `wicked-core`), `subdomain` its functional area (e.g. `core.run`). A `None` idempotency_key
/// means "always a new row" (a fresh UUID is minted); pass `Some(..)` for a deterministic, dedup-able
/// event (the bus enforces UNIQUE on the key).
#[derive(Debug, Clone)]
pub struct BusEmit {
    pub event_type: String,
    pub domain: String,
    pub subdomain: String,
    pub payload: serde_json::Value,
    pub idempotency_key: Option<String>,
    /// Per-event TTL override in hours (defaults to [`DEFAULT_TTL_HOURS`]).
    pub ttl_hours: Option<i64>,
}

impl BusEmit {
    /// Construct an event with the default 72h TTL and no fixed idempotency key.
    pub fn new(
        event_type: impl Into<String>,
        domain: impl Into<String>,
        subdomain: impl Into<String>,
        payload: serde_json::Value,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            domain: domain.into(),
            subdomain: subdomain.into(),
            payload,
            idempotency_key: None,
            ttl_hours: None,
        }
    }

    /// Pin a deterministic idempotency key so re-emitting the same logical event is a no-op dedup.
    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
}

/// One row read back from the bus `events` table.
#[derive(Debug, Clone)]
pub struct BusEvent {
    pub event_id: i64,
    pub event_type: String,
    pub domain: String,
    pub subdomain: String,
    pub payload: serde_json::Value,
}

/// A handle to a wicked-bus SQLite event log. Wraps a `rusqlite::Connection` so callers never name
/// `rusqlite` directly. Open one per thread that emits/polls — SQLite connections are not `Sync`.
pub struct BusDb {
    conn: Connection,
}

impl BusDb {
    /// Open (creating if absent) the bus db at `path`, apply the bus's PRAGMAs, and ensure the base
    /// `events` schema exists. Safe against a JS-created db (idempotent DDL) and safe for JS to open
    /// afterwards (JS layers its migration-3 nullable columns on top).
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open bus db at {path}"))?;
        // Match wicked-bus lib/db.js PRAGMAs. WAL + a busy timeout so a concurrent JS writer/sweeper
        // never trips us with SQLITE_BUSY.
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(EVENTS_DDL)
            .context("ensure wicked-bus events schema")?;
        Ok(Self { conn })
    }

    /// Publish `event`. Computes the bus's two-timer TTL (`expires_at = emitted_at + ttl`,
    /// `dedup_expires_at = emitted_at + 24h`) exactly as JS `emit()` does, so the JS poller's
    /// `expires_at > now` visibility check and 24h dedup sweep behave identically. Returns the new
    /// `event_id`. IDEMPOTENT: a duplicate idempotency key is not an error — the existing row's id is
    /// returned (the JS bus raises WB-002 for a hard dup; here a re-emit of a *deterministic* event is
    /// the normal at-least-once case, so we resolve to the existing row instead).
    pub fn emit(&self, event: &BusEmit) -> Result<i64> {
        let idempotency_key = event.idempotency_key.clone().unwrap_or_else(fresh_key);
        let emitted_at = now_ms();
        let ttl_hours = event.ttl_hours.unwrap_or(DEFAULT_TTL_HOURS);
        let expires_at = emitted_at + ttl_hours * MS_PER_HOUR;
        let dedup_expires_at = emitted_at + DEFAULT_DEDUP_TTL_HOURS * MS_PER_HOUR;
        let payload_str = serde_json::to_string(&event.payload)?;

        let res = self.conn.execute(
            "INSERT INTO events (event_type, domain, subdomain, payload, schema_version, \
             idempotency_key, emitted_at, expires_at, dedup_expires_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                event.event_type,
                event.domain,
                event.subdomain,
                payload_str,
                "1.0.0",
                idempotency_key,
                emitted_at,
                expires_at,
                dedup_expires_at,
            ],
        );
        match res {
            Ok(_) => Ok(self.conn.last_insert_rowid()),
            // A UNIQUE(idempotency_key) collision → the event is already on the bus. Resolve its id
            // (at-least-once idempotency, not a failure).
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == ErrorCode::ConstraintViolation =>
            {
                let id: i64 = self.conn.query_row(
                    "SELECT event_id FROM events WHERE idempotency_key = ?1",
                    [&idempotency_key],
                    |r| r.get(0),
                )?;
                Ok(id)
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Poll up to `batch` events matching `filter`, strictly after `after_event_id`, that have not
    /// expired. `after_event_id` is the caller's durable cursor floor (the JS bus persists an
    /// equivalent per-cursor `last_event_id`); the caller advances it past what it has handled. Rows
    /// are returned oldest-first. Filter grammar mirrors wicked-bus `matchesFilter` (see
    /// [`matches_filter`]).
    pub fn poll(&self, filter: &str, after_event_id: i64, batch: usize) -> Result<Vec<BusEvent>> {
        let now = now_ms();
        let mut stmt = self.conn.prepare(
            "SELECT event_id, event_type, domain, subdomain, payload FROM events \
             WHERE event_id > ?1 AND expires_at > ?2 ORDER BY event_id ASC",
        )?;
        let rows = stmt.query_map(rusqlite::params![after_event_id, now], |row| {
            let payload_str: String = row.get(4)?;
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                payload_str,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (event_id, event_type, domain, subdomain, payload_str) = row?;
            if !matches_filter(&event_type, &domain, filter) {
                continue;
            }
            let payload = serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null);
            out.push(BusEvent {
                event_id,
                event_type,
                domain,
                subdomain,
                payload,
            });
            if out.len() >= batch {
                break;
            }
        }
        Ok(out)
    }
}

/// A guaranteed-unique idempotency key for an event the caller did NOT pin one for (i.e. "always a new
/// row"). `pid`+`now_ms` alone collide when two keyless events are emitted in the same millisecond, so
/// a process-global monotonic counter is the disambiguator. Avoids pulling in a `uuid`/`rand` crate.
fn fresh_key() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("core-{}-{}-{}", std::process::id(), now_ms(), seq)
}

/// A deterministic idempotency key from parts (SHA-256 hex, truncated). Use for lifecycle events that
/// must dedup across at-least-once redelivery (e.g. `wicked.run.launched` keyed on the run id).
pub fn deterministic_key(parts: &[&str]) -> String {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p.as_bytes());
        h.update([0u8]);
    }
    let d = h.finalize();
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// Match an `event_type` (+ its `domain`) against a wicked-bus filter string. Mirrors
/// `lib/poll.js#matchesFilter`:
///  * `*` (usually `*@domain`) — catch-all.
///  * exact `event_type`.
///  * `prefix.**` — one-or-more trailing segments.
///  * `prefix.*` — exactly one trailing segment.
///  * a trailing `@domain` constrains the publisher.
pub fn matches_filter(event_type: &str, domain: &str, filter: &str) -> bool {
    let (type_pattern, domain_filter) = match filter.find('@') {
        Some(i) => (&filter[..i], Some(&filter[i + 1..])),
        None => (filter, None),
    };
    if let Some(df) = domain_filter {
        if domain != df {
            return false;
        }
    }
    if type_pattern == "*" {
        return true;
    }
    if type_pattern == event_type {
        return true;
    }
    if let Some(prefix) = type_pattern.strip_suffix(".**") {
        let lead = format!("{prefix}.");
        if let Some(rem) = event_type.strip_prefix(&lead) {
            return !rem.is_empty();
        }
        return false;
    }
    if let Some(prefix) = type_pattern.strip_suffix(".*") {
        let lead = format!("{prefix}.");
        if let Some(rem) = event_type.strip_prefix(&lead) {
            return !rem.is_empty() && !rem.contains('.');
        }
    }
    false
}

/// The event type the launch poller consumes.
pub const RUN_REQUESTED: &str = "wicked.run.requested";
/// The lifecycle event the launch poller emits back onto the bus when a run starts.
pub const RUN_LAUNCHED: &str = "wicked.run.launched";
/// The domain core publishes under.
pub const CORE_DOMAIN: &str = "wicked-core";

/// The `wicked.run.requested` payload contract: `{ workflow?, problem, args? }`.
#[derive(Debug, Deserialize)]
struct RunRequested {
    /// A registered `WorkflowDef` id (`feature`/`bug`/`migration`/drop-in). `None` ⇒ the free-text planner.
    workflow: Option<String>,
    /// The problem statement to decompose + run.
    problem: String,
    #[serde(default)]
    args: RunArgs,
}

#[derive(Debug, Default, Deserialize)]
struct RunArgs {
    /// A stable run id; defaults to `run-<event_id>` when omitted.
    session_id: Option<String>,
    /// A registered repo id to run within (creates an isolated worktree).
    repo_ref: Option<String>,
    /// `"all"` pauses before every unit; anything else ⇒ no human gate.
    human_confirm: Option<String>,
}

/// A running bridge: the poller thread plus its stop flag. Dropping it (or calling [`stop`]) signals
/// the thread to finish its current poll and join.
///
/// [`stop`]: BusBridge::stop
pub struct BusBridge {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl BusBridge {
    /// Signal the poller to stop and wait for it to exit.
    pub fn stop(mut self) {
        self.shutdown();
    }
    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for BusBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Spawn the launch bridge: a dedicated poller thread reading `wicked.run.requested` from the bus db
/// at `bus_db_path` and posting a `Command::LaunchRun` (built from each event) to the actor over `tx`.
/// Returns a [`BusBridge`] owning the thread. See the module docs for the actor-safety argument.
pub fn connect(
    tx: Sender<Command>,
    bus_db_path: impl Into<String>,
    roster: Vec<AgenticCli>,
    entity_mode: EntityMode,
    poll_interval: Duration,
) -> BusBridge {
    let stop = Arc::new(AtomicBool::new(false));
    let handle = spawn_run_requested_poller(
        bus_db_path.into(),
        tx,
        roster,
        entity_mode,
        poll_interval,
        stop.clone(),
    );
    BusBridge {
        stop,
        handle: Some(handle),
    }
}

/// The raw poller spawn (used by [`connect`] and by the actor's env-gated startup wiring). Runs until
/// `stop` is set OR the actor's command channel closes. Its cursor floor starts at the bus's current
/// MAX(event_id) — like a `cursor_init: latest` subscription — so it launches only requests emitted
/// AFTER the bridge came up, never replaying historical requests. At-least-once within a process run:
/// the floor advances only after a request has been posted to the actor, and the launch is idempotent
/// by run id (the actor rejects a re-plan of a live run), so a duplicate poll is harmless.
pub fn spawn_run_requested_poller(
    bus_db_path: String,
    tx: Sender<Command>,
    roster: Vec<AgenticCli>,
    entity_mode: EntityMode,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let db = match BusDb::open(&bus_db_path) {
            Ok(d) => d,
            Err(e) => {
                eprintln!(
                    "wicked-core: bus bridge disabled — cannot open bus db {bus_db_path}: {e}"
                );
                return;
            }
        };
        // Start at the tail: only NEW requests drive launches.
        let mut floor: i64 = db
            .conn
            .query_row("SELECT COALESCE(MAX(event_id), 0) FROM events", [], |r| {
                r.get(0)
            })
            .unwrap_or(0);
        let filter = RUN_REQUESTED; // exact-match filter

        while !stop.load(Ordering::SeqCst) {
            let events = match db.poll(filter, floor, 100) {
                Ok(evs) => evs,
                Err(e) => {
                    eprintln!("wicked-core: bus poll error: {e}");
                    Vec::new()
                }
            };
            for ev in events {
                match launch_from_event(&tx, &db, &roster, entity_mode, &ev) {
                    Ok(()) => {}
                    Err(BridgeError::ActorGone) => return, // the Core dropped — nothing to feed
                    Err(BridgeError::Other(e)) => {
                        eprintln!(
                            "wicked-core: bus bridge could not launch run for event {}: {e}",
                            ev.event_id
                        );
                    }
                }
                // Advance PAST this request only after we've attempted the launch (at-least-once).
                floor = ev.event_id;
            }
            // Sleep in short slices so `stop` is honored promptly.
            let mut slept = Duration::ZERO;
            let slice = Duration::from_millis(50);
            while slept < poll_interval && !stop.load(Ordering::SeqCst) {
                std::thread::sleep(slice);
                slept += slice;
            }
        }
    })
}

enum BridgeError {
    /// The actor's command receiver is gone (Core dropped) — the poller should exit.
    ActorGone,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for BridgeError {
    fn from(e: anyhow::Error) -> Self {
        BridgeError::Other(e)
    }
}

/// Turn one `wicked.run.requested` event into a `LaunchRun` posted to the actor, then emit
/// `wicked.run.launched` back onto the bus (proof of the publish path). Errors that mean "the run
/// already exists" (idempotent redelivery) are swallowed — the launched event is still (idempotently)
/// emitted so downstream consumers see a stable signal.
fn launch_from_event(
    tx: &Sender<Command>,
    db: &BusDb,
    roster: &[AgenticCli],
    entity_mode: EntityMode,
    ev: &BusEvent,
) -> Result<(), BridgeError> {
    let req: RunRequested = serde_json::from_value(ev.payload.clone())
        .with_context(|| format!("parse {RUN_REQUESTED} payload"))?;
    let session_id = req
        .args
        .session_id
        .clone()
        .unwrap_or_else(|| format!("run-{}", ev.event_id));
    let human_confirm = match req.args.human_confirm.as_deref() {
        Some("all") => HumanConfirm::All,
        _ => HumanConfirm::None,
    };
    let spec = LaunchSpec {
        problem: req.problem.clone(),
        clis: roster.to_vec(),
        entity_mode,
        session_id: session_id.clone(),
        human_confirm,
        repo_ref: req.args.repo_ref.clone(),
        workflow: req.workflow.clone(),
    };

    let (reply, rx) = channel();
    if tx.send(Command::LaunchRun { spec, reply }).is_err() {
        return Err(BridgeError::ActorGone);
    }
    // The actor plans+distributes synchronously then replies with the run id. Blocking here is fine —
    // this is the poller thread, never the actor thread. A disconnected reply means the actor is gone.
    match rx.recv() {
        Ok(Ok(run_id)) => {
            emit_run_launched(db, &run_id, req.workflow.as_deref(), &req.problem);
        }
        Ok(Err(e)) => {
            // "already exists" ⇒ an idempotent redelivery; still surface a stable launched signal.
            if e.to_string().contains("already exists") {
                emit_run_launched(db, &session_id, req.workflow.as_deref(), &req.problem);
            } else {
                return Err(BridgeError::Other(e));
            }
        }
        Err(_) => return Err(BridgeError::ActorGone),
    }
    Ok(())
}

/// Emit `wicked.run.launched {run_id, workflow, problem}` — deterministically keyed on the run id so
/// at-least-once redelivery dedups to one row. Best-effort: a failed publish is logged, never fatal.
fn emit_run_launched(db: &BusDb, run_id: &str, workflow: Option<&str>, problem: &str) {
    let payload = serde_json::json!({
        "run_id": run_id,
        "workflow": workflow,
        "problem": problem,
    });
    let ev = BusEmit::new(RUN_LAUNCHED, CORE_DOMAIN, "core.run", payload)
        .with_key(deterministic_key(&[RUN_LAUNCHED, run_id]));
    if let Err(e) = db.emit(&ev) {
        eprintln!("wicked-core: failed to emit {RUN_LAUNCHED} for {run_id}: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_bus(name: &str) -> String {
        let dir =
            std::env::temp_dir().join(format!("wicked-core-bus-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("bus.db").to_str().unwrap().to_string()
    }

    #[test]
    fn emit_then_poll_roundtrips_within_rust() {
        let db = BusDb::open(&tmp_bus("rt")).unwrap();
        let id = db
            .emit(&BusEmit::new(
                RUN_REQUESTED,
                "wicked-cli",
                "cli.run",
                serde_json::json!({ "workflow": "feature", "problem": "add SSO" }),
            ))
            .unwrap();
        assert!(id > 0);
        let got = db.poll(RUN_REQUESTED, 0, 10).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].event_id, id);
        assert_eq!(got[0].event_type, RUN_REQUESTED);
        assert_eq!(got[0].payload["problem"], "add SSO");
        // The floor is exclusive: polling past this id yields nothing.
        assert!(db.poll(RUN_REQUESTED, id, 10).unwrap().is_empty());
    }

    #[test]
    fn duplicate_idempotency_key_resolves_to_the_same_row() {
        let db = BusDb::open(&tmp_bus("idem")).unwrap();
        let ev = BusEmit::new(
            RUN_LAUNCHED,
            CORE_DOMAIN,
            "core.run",
            serde_json::json!({"run_id":"r1"}),
        )
        .with_key(deterministic_key(&[RUN_LAUNCHED, "r1"]));
        let a = db.emit(&ev).unwrap();
        let b = db.emit(&ev).unwrap();
        assert_eq!(
            a, b,
            "a re-emit of a pinned-key event dedups to the same id"
        );
        let count: i64 = db
            .conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "only one physical row exists");
    }

    #[test]
    fn poll_respects_filter_and_expiry() {
        let db = BusDb::open(&tmp_bus("filter")).unwrap();
        db.emit(&BusEmit::new(
            RUN_REQUESTED,
            "d",
            "s",
            serde_json::json!({}),
        ))
        .unwrap();
        db.emit(&BusEmit::new(
            "wicked.skill.needed",
            "d",
            "s",
            serde_json::json!({}),
        ))
        .unwrap();
        // Exact filter picks only the run.requested event.
        assert_eq!(db.poll(RUN_REQUESTED, 0, 10).unwrap().len(), 1);
        // Multi-level wildcard picks both.
        assert_eq!(db.poll("wicked.**", 0, 10).unwrap().len(), 2);
        // Domain-constrained catch-all.
        assert_eq!(db.poll("*@d", 0, 10).unwrap().len(), 2);
        assert_eq!(db.poll("*@other", 0, 10).unwrap().len(), 0);
        // An already-expired event is invisible to poll.
        let expired = BusEmit {
            ttl_hours: Some(-1), // expires_at = emitted_at - 1h  →  < now
            ..BusEmit::new("wicked.old.event", "d", "s", serde_json::json!({}))
        };
        db.emit(&expired).unwrap();
        assert!(db.poll("wicked.old.event", 0, 10).unwrap().is_empty());
    }

    #[test]
    fn filter_grammar_matches_the_js_bus() {
        assert!(matches_filter(
            "wicked.run.requested",
            "d",
            "wicked.run.requested"
        ));
        assert!(matches_filter("wicked.run.requested", "d", "wicked.run.*"));
        assert!(!matches_filter(
            "wicked.run.requested.v2",
            "d",
            "wicked.run.*"
        )); // single-level only
        assert!(matches_filter(
            "wicked.run.requested.v2",
            "d",
            "wicked.run.**"
        ));
        assert!(matches_filter("anything", "d", "*"));
        assert!(matches_filter("x.y.z", "core", "*@core"));
        assert!(!matches_filter("x.y.z", "other", "*@core"));
        assert!(matches_filter(
            "wicked.run.requested",
            "core",
            "wicked.run.requested@core"
        ));
        assert!(!matches_filter(
            "wicked.run.requested",
            "other",
            "wicked.run.requested@core"
        ));
    }
}
