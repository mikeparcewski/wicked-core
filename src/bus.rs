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
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, ErrorCode, OptionalExtension};
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

/// The subset of the wicked-bus `config.json` the bridge cares about: the two TTL timers JS `emit()`
/// reads (`config.ttl_hours` / `config.dedup_ttl_hours`). Every other key is ignored so operator/CLI
/// additions to the file never break parsing (forward-compatible, mirroring JS `loadConfig`).
#[derive(Debug, Deserialize)]
struct BusConfig {
    ttl_hours: Option<i64>,
    dedup_ttl_hours: Option<i64>,
}

/// Resolve the wicked-bus data dir the way JS `paths.js`/`config.js` do — for the sole purpose of
/// locating `config.json`. `WICKED_BUS_DATA_DIR` wins if set (matches `resolveDataDir()`); otherwise
/// the directory CONTAINING the bus db (the default layout is `<dataDir>/bus.db`, so the db's parent
/// IS the data dir). `None` when neither yields a directory (a bare-filename db path) → caller uses
/// the built-in defaults, exactly as JS falls back when the file is absent.
fn resolve_config_dir(db_path: &str) -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os("WICKED_BUS_DATA_DIR") {
        if !dir.is_empty() {
            return Some(std::path::PathBuf::from(dir));
        }
    }
    std::path::Path::new(db_path)
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
}

/// Load `(ttl_hours, dedup_ttl_hours)` from `<dataDir>/config.json`, matching JS `loadConfig()`: a
/// missing file or malformed JSON silently yields the defaults (72/24), and an absent key falls back
/// to its own default. This is what makes the Rust two-timer TTL agree with JS under a NON-default
/// operator config — otherwise the JS sweep (which deletes on `dedup_expires_at`) reaps Rust rows on a
/// different clock than JS-written rows.
fn load_bus_ttls(db_path: &str) -> (i64, i64) {
    let cfg = resolve_config_dir(db_path)
        .map(|dir| dir.join("config.json"))
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str::<BusConfig>(&raw).ok());
    match cfg {
        Some(c) => (
            c.ttl_hours.unwrap_or(DEFAULT_TTL_HOURS),
            c.dedup_ttl_hours.unwrap_or(DEFAULT_DEDUP_TTL_HOURS),
        ),
        None => (DEFAULT_TTL_HOURS, DEFAULT_DEDUP_TTL_HOURS),
    }
}

/// A handle to a wicked-bus SQLite event log. Wraps a `rusqlite::Connection` so callers never name
/// `rusqlite` directly. Open one per thread that emits/polls — SQLite connections are not `Sync`.
pub struct BusDb {
    conn: Connection,
    /// Default event TTL in hours (`config.ttl_hours`), loaded from the wicked-bus `config.json` at
    /// open time (falls back to [`DEFAULT_TTL_HOURS`]). Threaded into [`emit`] so `expires_at` matches
    /// what JS `emit()` computes under the SAME operator config.
    default_ttl_hours: i64,
    /// Dedup-row TTL in hours (`config.dedup_ttl_hours`), same provenance as `default_ttl_hours`
    /// (falls back to [`DEFAULT_DEDUP_TTL_HOURS`]). Drives `dedup_expires_at`, which the JS sweep
    /// deletes on — so this must agree with JS or Rust rows are reaped early/late.
    dedup_ttl_hours: i64,
}

impl BusDb {
    /// Open (creating if absent) the bus db at `path`, apply the bus's PRAGMAs, and ensure the base
    /// `events` schema exists. Safe against a JS-created db (idempotent DDL) and safe for JS to open
    /// afterwards (JS layers its migration-3 nullable columns on top). Also loads the operator's
    /// `ttl_hours`/`dedup_ttl_hours` from `config.json` so emitted rows carry the SAME two-timer TTL a
    /// JS `emit()` would under that config.
    pub fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("open bus db at {path}"))?;
        // Match wicked-bus lib/db.js PRAGMAs. WAL + a busy timeout so a concurrent JS writer/sweeper
        // never trips us with SQLITE_BUSY.
        conn.busy_timeout(Duration::from_millis(5000))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(EVENTS_DDL)
            .context("ensure wicked-bus events schema")?;
        let (default_ttl_hours, dedup_ttl_hours) = load_bus_ttls(path);
        Ok(Self {
            conn,
            default_ttl_hours,
            dedup_ttl_hours,
        })
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
        // Per-event TTL override wins, else the operator config's `ttl_hours` (mirrors JS `emit`:
        // `event.ttl_hours != null ? event.ttl_hours : config.ttl_hours`).
        let ttl_hours = event.ttl_hours.unwrap_or(self.default_ttl_hours);
        let expires_at = emitted_at + ttl_hours * MS_PER_HOUR;
        let dedup_expires_at = emitted_at + self.dedup_ttl_hours * MS_PER_HOUR;
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
            // (at-least-once idempotency, not a failure). Narrowed to the UNIQUE extended code: a
            // CHECK(length)/NOT NULL breach is ALSO a `ConstraintViolation`, but it is NOT a dedup —
            // the by-key SELECT would then find no row and we'd surface a confusing
            // `QueryReturnedNoRows` instead of the real constraint error. So we (a) only treat the
            // UNIQUE code as dedup, and (b) if the by-key SELECT unexpectedly finds nothing, propagate
            // the ORIGINAL constraint error rather than a no-rows error.
            Err(rusqlite::Error::SqliteFailure(e, msg))
                if e.code == ErrorCode::ConstraintViolation
                    && e.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                let existing: Option<i64> = self
                    .conn
                    .query_row(
                        "SELECT event_id FROM events WHERE idempotency_key = ?1",
                        [&idempotency_key],
                        |r| r.get(0),
                    )
                    .optional()?;
                match existing {
                    Some(id) => Ok(id),
                    None => Err(rusqlite::Error::SqliteFailure(e, msg).into()),
                }
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
    // JS treats an empty domain segment (`type@` / a bare `@`) as a FALSY filter and skips the domain
    // check entirely (`if (domainFilter && domain !== domainFilter)`). Mirror that truthiness: only a
    // non-empty domain segment constrains the publisher.
    if let Some(df) = domain_filter {
        if !df.is_empty() && domain != df {
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
///
/// ## The floor is snapshotted SYNCHRONOUSLY (a happens-before guarantee, not a race)
/// The cursor floor is read from the bus db HERE, on the caller's thread, BEFORE the poller thread is
/// spawned — not lazily inside the thread. This is load-bearing: a caller that connects the bridge and
/// THEN emits a request (the `connect_bus` → `emit` ordering the bridge is designed around) is
/// guaranteed that the floor reflects only events that existed at connect time, so the just-emitted
/// request always has `event_id > floor` and is delivered. Reading the floor lazily on the spawned
/// thread made this a race: under load the emit could win, the thread would then read `MAX(event_id)`
/// as the just-emitted id, set the floor PAST it, and silently drop the only request forever.
pub fn spawn_run_requested_poller(
    bus_db_path: String,
    tx: Sender<Command>,
    roster: Vec<AgenticCli>,
    entity_mode: EntityMode,
    poll_interval: Duration,
    stop: Arc<AtomicBool>,
) -> JoinHandle<()> {
    // Snapshot the tail SYNCHRONOUSLY on the caller's thread (see the doc comment): once this function
    // returns, any subsequently-emitted request has a strictly higher id than the floor. If the
    // snapshot CANNOT be read here, the floor is UNKNOWN — we must NOT fall back to 0. Starting at 0
    // would replay EVERY non-expired historical `run.requested` (up to the TTL window, ~72h) as a fresh
    // launch: a mass-duplicate storm. Instead treat the bridge as un-initialized and disable it (the
    // same posture as the thread-side open-failure path below), spawning a thread that just exits.
    let floor_init: Option<i64> = BusDb::open(&bus_db_path).ok().and_then(|db| {
        db.conn
            .query_row("SELECT COALESCE(MAX(event_id), 0) FROM events", [], |r| {
                r.get(0)
            })
            .ok()
    });
    let floor_init = match floor_init {
        Some(f) => f,
        None => {
            eprintln!(
                "wicked-core: bus bridge disabled — cannot snapshot cursor floor from bus db \
                 {bus_db_path}; refusing to replay history"
            );
            return std::thread::spawn(|| {});
        }
    };
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
        // Start at the tail SNAPSHOTTED BEFORE SPAWN (above): only requests newer than connect-time
        // drive launches, and a request emitted right after connect is never missed.
        let mut floor: i64 = floor_init;
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
                match launch_from_event(&tx, &db, &roster, entity_mode, &ev, &stop) {
                    // Handled (launched OR idempotent redelivery) → advance PAST this request.
                    Ok(()) => {
                        floor = ev.event_id;
                    }
                    // The Core dropped, or we're stopping — exit the thread so its `join()` returns.
                    Err(BridgeError::ActorGone) | Err(BridgeError::Stopped) => return,
                    // Poison payload → advance past it (retrying is futile; don't wedge the batch).
                    Err(BridgeError::Permanent(e)) => {
                        eprintln!(
                            "wicked-core: bus bridge dropping unprocessable event {} (permanent): {e}",
                            ev.event_id
                        );
                        floor = ev.event_id;
                    }
                    // Transient fault → do NOT advance the floor; break the batch and re-poll after the
                    // sleep so this request is RETRIED (at-least-once, not at-most-once).
                    Err(BridgeError::Retriable(e)) => {
                        eprintln!(
                            "wicked-core: bus bridge could not launch run for event {} (transient, \
                             will retry): {e}",
                            ev.event_id
                        );
                        break;
                    }
                }
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
    /// The stop flag was observed while waiting for the actor's reply — the poller should exit.
    Stopped,
    /// A TRANSIENT store/IO/worktree/dispatch fault. The event was NOT handled: do NOT advance the
    /// floor, re-poll after the sleep so it is retried (at-least-once). This is the retriable class.
    Retriable(anyhow::Error),
    /// A PERMANENT fault — a payload that can never parse (poison). Retrying is futile, so the floor
    /// IS advanced past it (dropping the poison row) rather than wedging the batch forever.
    Permanent(anyhow::Error),
}

/// Turn one `wicked.run.requested` event into a `LaunchRun` posted to the actor, then emit
/// `wicked.run.launched` back onto the bus (proof of the publish path). Idempotent redeliveries (the
/// run is already in flight — [`RunBusy`] — or already exists non-terminally — [`RunExists`]) are
/// treated as SUCCESS, not failure: the launched event is still (idempotently) emitted so downstream
/// consumers see a stable signal, and the poller advances its floor past the request.
///
/// Error classification (drives whether the caller advances the floor):
///  * payload can never parse ⇒ [`BridgeError::Permanent`] (poison — advance past it).
///  * a genuine store/IO/worktree/dispatch fault ⇒ [`BridgeError::Retriable`] (do NOT advance — retry).
///  * actor channel closed ⇒ [`BridgeError::ActorGone`]; stop flag observed ⇒ [`BridgeError::Stopped`].
///
/// `stop` is polled while waiting for the actor's reply so a shutting-down actor (which will never
/// reply) cannot wedge this thread in an un-cancellable `recv()` — that deadlocked the actor's
/// `join()` on the poller at shutdown.
fn launch_from_event(
    tx: &Sender<Command>,
    db: &BusDb,
    roster: &[AgenticCli],
    entity_mode: EntityMode,
    ev: &BusEvent,
    stop: &Arc<AtomicBool>,
) -> Result<(), BridgeError> {
    // A malformed payload is a PERMANENT poison — it will never parse, so retrying it forever would
    // wedge every later request behind it. Advance past it instead.
    let req: RunRequested = serde_json::from_value(ev.payload.clone())
        .with_context(|| format!("parse {RUN_REQUESTED} payload"))
        .map_err(BridgeError::Permanent)?;
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
    // this is the poller thread, never the actor thread — BUT the wait MUST be cancellable: at
    // shutdown the actor breaks its loop without replying, so an un-bounded `recv()` here would hang
    // forever and the actor's `join()` on this thread would deadlock. Poll `stop` on a short timeout.
    let reply = loop {
        if stop.load(Ordering::SeqCst) {
            return Err(BridgeError::Stopped);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(r) => break r,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return Err(BridgeError::ActorGone),
        }
    };
    match reply {
        Ok(run_id) => {
            emit_run_launched(db, &run_id, req.workflow.as_deref(), &req.problem);
            Ok(())
        }
        Err(e) => {
            // TYPED idempotency: a run already in flight (RunBusy) or an existing non-terminal run
            // (RunExists) is a duplicate REQUEST, not a fault. Both dedup to the live run — surface a
            // stable launched signal and let the caller advance the floor (no retry). Anything else is
            // a genuine transient fault → retriable (do NOT advance the floor).
            if e.downcast_ref::<crate::RunBusy>().is_some()
                || e.downcast_ref::<crate::RunExists>().is_some()
            {
                emit_run_launched(db, &session_id, req.workflow.as_deref(), &req.problem);
                Ok(())
            } else {
                Err(BridgeError::Retriable(e))
            }
        }
    }
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
        // An EMPTY domain segment (a trailing `@`, or a bare `@`) is a FALSY domain filter in JS —
        // the domain check is skipped, so any publisher matches. Rust used to reject every non-empty
        // domain here (finding #6).
        assert!(matches_filter(
            "wicked.run.requested",
            "any-domain",
            "wicked.run.requested@"
        ));
        assert!(matches_filter("x.y.z", "core", "x.y.z@"));
        assert!(matches_filter("anything", "whatever", "*@"));
    }

    /// Finding #1: under a NON-default operator `config.json` (next to the bus db) the two-timer TTL
    /// must be read from it — not hardcoded to 72/24 — so Rust rows carry the same `expires_at` /
    /// `dedup_expires_at` a JS `emit()` would compute (otherwise the JS sweep reaps Rust rows on a
    /// different clock).
    #[test]
    fn emit_uses_config_json_ttls() {
        let path = tmp_bus("cfgttl");
        let dir = std::path::Path::new(&path).parent().unwrap();
        std::fs::write(
            dir.join("config.json"),
            r#"{"ttl_hours": 5, "dedup_ttl_hours": 3}"#,
        )
        .unwrap();
        let db = BusDb::open(&path).unwrap();
        assert_eq!(db.default_ttl_hours, 5, "ttl_hours read from config.json");
        assert_eq!(
            db.dedup_ttl_hours, 3,
            "dedup_ttl_hours read from config.json"
        );

        let before = now_ms();
        let id = db
            .emit(&BusEmit::new(
                RUN_REQUESTED,
                "d",
                "s",
                serde_json::json!({}),
            ))
            .unwrap();
        let (expires_at, dedup_expires_at): (i64, i64) = db
            .conn
            .query_row(
                "SELECT expires_at, dedup_expires_at FROM events WHERE event_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // 5h / 3h from emitted_at (~`before`), NOT the 72h/24h defaults.
        assert!(
            expires_at >= before + 5 * MS_PER_HOUR && expires_at < before + 6 * MS_PER_HOUR,
            "expires_at reflects config ttl_hours=5, got {expires_at} (before={before})"
        );
        assert!(
            dedup_expires_at >= before + 3 * MS_PER_HOUR
                && dedup_expires_at < before + 4 * MS_PER_HOUR,
            "dedup_expires_at reflects config dedup_ttl_hours=3"
        );
    }

    /// An absent `config.json` (or one missing the keys) falls back to the built-in 72/24 defaults,
    /// mirroring JS `loadConfig()`.
    #[test]
    fn emit_falls_back_to_default_ttls_without_config() {
        let db = BusDb::open(&tmp_bus("noconfig")).unwrap();
        assert_eq!(db.default_ttl_hours, DEFAULT_TTL_HOURS);
        assert_eq!(db.dedup_ttl_hours, DEFAULT_DEDUP_TTL_HOURS);
    }

    // ── Bridge error-classification + retry tests (findings #2/#9, #4, #5) ──────────────────────────
    // These drive the real poller / `launch_from_event` against a FAKE actor (a plain
    // `Receiver<Command>` loop) so no store/engine is needed. The fake actor's reply models each
    // outcome: transient fault, RunBusy/RunExists idempotency, or success.

    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::Receiver;

    fn fake_actor<F>(rx: Receiver<Command>, mut handler: F) -> JoinHandle<()>
    where
        F: FnMut(usize) -> anyhow::Result<String> + Send + 'static,
    {
        std::thread::spawn(move || {
            let mut n = 0usize;
            while let Ok(cmd) = rx.recv() {
                if let Command::LaunchRun { reply, .. } = cmd {
                    let res = handler(n);
                    n += 1;
                    let _ = reply.send(res);
                }
            }
        })
    }

    fn requested(event_id: i64, session: &str) -> BusEvent {
        BusEvent {
            event_id,
            event_type: RUN_REQUESTED.to_string(),
            domain: "d".to_string(),
            subdomain: "s".to_string(),
            payload: serde_json::json!({ "problem": "p", "args": { "session_id": session } }),
        }
    }

    /// Finding #2/#9: a TRANSIENT launch fault is surfaced as `Retriable` (which the poller uses to
    /// NOT advance the floor), and a later attempt on the SAME event succeeds — at-least-once, not
    /// at-most-once.
    #[test]
    fn transient_launch_failure_is_retriable_then_succeeds() {
        let db = BusDb::open(&tmp_bus("retry")).unwrap();
        let (tx, rx) = channel();
        let actor = fake_actor(rx, |n| {
            if n == 0 {
                anyhow::bail!("transient store/worktree fault")
            } else {
                Ok("run-x".to_string())
            }
        });
        let stop = Arc::new(AtomicBool::new(false));
        let ev = requested(1, "run-x");

        let r1 = launch_from_event(&tx, &db, &[], EntityMode::Shared, &ev, &stop);
        assert!(
            matches!(r1, Err(BridgeError::Retriable(_))),
            "a transient fault is retriable (floor must not advance)"
        );
        // No launched signal yet — the run never started.
        assert_eq!(db.poll(RUN_LAUNCHED, 0, 10).unwrap().len(), 0);

        let r2 = launch_from_event(&tx, &db, &[], EntityMode::Shared, &ev, &stop);
        assert!(r2.is_ok(), "the retry succeeds");
        assert_eq!(
            db.poll(RUN_LAUNCHED, 0, 10).unwrap().len(),
            1,
            "launched emitted only after success"
        );

        drop(tx);
        actor.join().unwrap();
    }

    /// Finding #4: a duplicate request against a still-running run (actor replies `RunBusy`) — and an
    /// existing non-terminal run (`RunExists`) — are IDEMPOTENT redeliveries, not faults. Both resolve
    /// to `Ok` and still emit a stable `run.launched` signal.
    #[test]
    fn runbusy_and_runexists_duplicates_are_idempotent() {
        let db = BusDb::open(&tmp_bus("idembusy")).unwrap();
        let (tx, rx) = channel();
        let actor = fake_actor(rx, |n| {
            if n == 0 {
                Err(crate::RunBusy("run-dup".to_string()).into())
            } else {
                Err(crate::RunExists("run-dup".to_string(), "Executing".to_string()).into())
            }
        });
        let stop = Arc::new(AtomicBool::new(false));
        let ev = requested(7, "run-dup");

        let r_busy = launch_from_event(&tx, &db, &[], EntityMode::Shared, &ev, &stop);
        assert!(
            r_busy.is_ok(),
            "RunBusy is an idempotent redelivery, not a fault"
        );

        let r_exists = launch_from_event(&tx, &db, &[], EntityMode::Shared, &ev, &stop);
        assert!(
            r_exists.is_ok(),
            "RunExists is an idempotent redelivery, not a fault"
        );

        // Both dedup to the same run id → the launched event is keyed on it and appears once.
        let launched = db.poll(RUN_LAUNCHED, 0, 10).unwrap();
        assert_eq!(
            launched.len(),
            1,
            "one stable launched signal (deterministic key)"
        );
        assert_eq!(launched[0].payload["run_id"], "run-dup");

        drop(tx);
        actor.join().unwrap();
    }

    /// Finding #2/#9 at the POLLER level: a transient failure does NOT advance the floor, so the SAME
    /// emitted request is re-attempted (>= 2 LaunchRun commands reach the actor) and eventually
    /// launches — proving the event is never silently dropped.
    #[test]
    fn poller_retries_transient_failure_without_dropping() {
        let path = tmp_bus("pollerretry");
        let (tx, rx) = channel();
        let attempts = Arc::new(AtomicUsize::new(0));
        let a2 = attempts.clone();
        let actor = std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                if let Command::LaunchRun { reply, .. } = cmd {
                    let n = a2.fetch_add(1, Ordering::SeqCst);
                    let res: anyhow::Result<String> = if n == 0 {
                        Err(anyhow::anyhow!("transient"))
                    } else {
                        Ok("run-1".to_string())
                    };
                    let _ = reply.send(res);
                }
            }
        });
        let stop = Arc::new(AtomicBool::new(false));
        // Connect BEFORE emitting: floor snapshot = 0 on the empty db, so the request (id 1 > 0) is seen.
        let handle = spawn_run_requested_poller(
            path.clone(),
            tx,
            vec![],
            EntityMode::Shared,
            Duration::from_millis(20),
            stop.clone(),
        );
        let db = BusDb::open(&path).unwrap();
        db.emit(&BusEmit::new(
            RUN_REQUESTED,
            "d",
            "s",
            serde_json::json!({ "problem": "p", "args": { "session_id": "run-1" } }),
        ))
        .unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while attempts.load(Ordering::SeqCst) < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            attempts.load(Ordering::SeqCst) >= 2,
            "the transient failure did not advance the floor → the request was retried"
        );

        // After the retry succeeds, exactly one run.launched lands.
        let deadline2 = std::time::Instant::now() + Duration::from_secs(3);
        let mut launched = 0;
        while std::time::Instant::now() < deadline2 {
            launched = db.poll(RUN_LAUNCHED, 0, 10).unwrap().len();
            if launched >= 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(
            launched, 1,
            "launched emitted once after the retry succeeds"
        );

        stop.store(true, Ordering::SeqCst);
        handle.join().unwrap();
        drop(db);
        actor.join().unwrap();
    }

    /// Finding #5: the poller's wait for the actor's reply is CANCELLABLE. A shutting-down actor never
    /// replies; the poller must still observe the stop flag and exit so the actor's `join()` on it
    /// returns instead of deadlocking. The fake actor here receives the LaunchRun but NEVER replies.
    #[test]
    fn poller_reply_wait_is_cancellable_on_stop() {
        let path = tmp_bus("cancelwait");
        let (tx, rx) = channel::<Command>();
        let held: Arc<std::sync::Mutex<Vec<std::sync::mpsc::Sender<anyhow::Result<String>>>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let held2 = held.clone();
        let actor = std::thread::spawn(move || {
            while let Ok(cmd) = rx.recv() {
                if let Command::LaunchRun { reply, .. } = cmd {
                    held2.lock().unwrap().push(reply); // deliberately never reply
                }
            }
        });
        let stop = Arc::new(AtomicBool::new(false));
        let handle = spawn_run_requested_poller(
            path.clone(),
            tx,
            vec![],
            EntityMode::Shared,
            Duration::from_millis(20),
            stop.clone(),
        );
        let db = BusDb::open(&path).unwrap();
        db.emit(&BusEmit::new(
            RUN_REQUESTED,
            "d",
            "s",
            serde_json::json!({ "problem": "p" }),
        ))
        .unwrap();
        // Let the poller send LaunchRun and block in the bounded reply-wait.
        std::thread::sleep(Duration::from_millis(250));
        stop.store(true, Ordering::SeqCst);

        // The poller MUST exit promptly (bounded-join to avoid hanging the suite on a regression).
        let (done_tx, done_rx) = channel();
        std::thread::spawn(move || {
            let _ = handle.join();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_secs(3)).is_ok(),
            "poller exited after stop despite an un-answered reply (no deadlock)"
        );

        drop(db);
        drop(held);
        actor.join().unwrap();
    }
}
