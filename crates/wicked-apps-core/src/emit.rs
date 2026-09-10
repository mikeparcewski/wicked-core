//! Shared event-emit seam for the four apps — the NATIVE path every app calls to publish a
//! `wicked.*` event onto the shared estate store. NO Node `wicked-bus` subprocess.
//!
//! ## v0.2.0 — native, store-backed eventing
//! The Rust collection's toolbox is Rust-only and the shared estate store IS the integration
//! substrate, so eventing no longer shells out to the Node `wicked-bus` CLI. An event is written as
//! a coarse `Node(NodeKind::Other(`[`EVENT`](crate::EVENT)`))` on the store:
//! - [`emit_event_to`] — the caller passes the store handle it ALREADY holds (no second connection,
//!   no process spawn). PREFER this on any path that already has a store open.
//! - [`emit_event`] — no store handle: resolves the shared store from [`ESTATE_DB_ENV`] and writes
//!   there; if the env is unset (tests, ephemeral/in-memory scope) it appends the event to a local
//!   append-only outbox spool instead. Still never spawns a subprocess.
//!
//! Emit is fire-and-forget by design (it must never block or fail the caller), but never silent: a
//! failed store write falls back to the outbox spool (NDJSON) with a loud [`DEADLETTER_MARKER`] on
//! stderr. A dropped event is a defect, never silent.
//!
//! Events are coarse + off the hot path (counts/ids). They are queryable from the store via
//! `find_symbols(kind = EVENT)` and ordered by the timestamp-prefixed node id (a `changes_since`-
//! style cursor drain can layer on later).
//!
//! ## Cross-platform
//! The spool root resolves via `std::env::var_os("HOME")` / `USERPROFILE` joined with
//! `std::path::Path` segments (never a hardcoded `~`), overridable via [`DEADLETTER_ENV`].

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use wicked_estate_core::SymbolQuery;

use crate::{
    open_store, synthetic_symbol, GraphRead, GraphStore, Language, Location, Node, NodeKind, Span,
    ESTATE_DB_ENV, EVENT, SYMBOL_SCHEME,
};

/// Overrides the outbox / dead-letter spool file path. When unset, the spool defaults to
/// `<home>/.something-wicked/wicked-apps/emit-outbox.ndjson`.
pub const DEADLETTER_ENV: &str = "WICKED_APPS_EMIT_DEADLETTER";

/// Optional origin stamp for spooled records (wicked-crew#495): the launcher that owns this
/// process sets it to a human-readable "who am I" (wicked-crew `serve` writes
/// `wicked-crew@<version> serve pid=<pid> port=<port> db=<core db>`), and every dead letter carries
/// it as `origin` beside the engine's own `ts` and `pid` — so a record read months later, in a
/// drained outbox, still says WHICH daemon could not store it. Unset → no `origin` key.
pub const ORIGIN_ENV: &str = "WICKED_APPS_EMIT_ORIGIN";

/// Loud, greppable marker written to stderr whenever an event is spooled instead of stored.
pub const DEADLETTER_MARKER: &str = "EMIT-DEADLETTER:";

/// Process-local monotonic counter, mixed into the event node id so concurrent emits never collide.
static EMIT_SEQ: AtomicU64 = AtomicU64::new(0);

/// A coarse wicked event ready to publish through the shared seam.
///
/// `event_type` follows the ecosystem convention `wicked.<noun>.<verb>` (validate with
/// [`crate::validate_event_type`] before constructing). `payload` is an already-built JSON object.
#[derive(Debug, Clone)]
pub struct EmitEvent {
    /// `wicked.<domain>.<noun>.<verb>` — e.g. `wicked.crew.policy.evaluated`.
    pub event_type: String,
    /// Top-level domain — e.g. `wicked-governance`.
    pub domain: String,
    /// Subdomain — e.g. `governance.evaluation`.
    pub subdomain: String,
    /// Structured event payload (a JSON object).
    pub payload: serde_json::Value,
}

impl EmitEvent {
    /// Construct an event. `domain` is the producing app (e.g. `wicked-governance`); `subdomain`
    /// is the dotted subdomain (e.g. `governance.evaluation`); `event_type` is the full
    /// `wicked.<noun>.<verb>` name.
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
        }
    }

    /// The outbox record: the envelope plus the reason it was spooled, stamped with WHEN (`ts`,
    /// epoch milliseconds — the CoreEvent convention) and BY WHOM (`pid`, and `origin` when the
    /// launcher set [`ORIGIN_ENV`]). Serialized as one NDJSON line. Before the stamps (crew#495 /
    /// F-022) a 3,400-entry outbox had no recoverable order and no way to tell two daemons' entries
    /// apart; [`replay_outbox`] restores `ts` onto the replayed node so an id-ordered scan of the
    /// store is still chronological.
    fn spool_record(&self, reason: &str) -> serde_json::Value {
        let mut record = serde_json::json!({
            "type": self.event_type,
            "domain": self.domain,
            "subdomain": self.subdomain,
            "payload": self.payload,
            "deadletter_reason": reason,
            "ts": now_millis(),
            "pid": std::process::id(),
        });
        if let Some(origin) = std::env::var_os(ORIGIN_ENV).filter(|o| !o.is_empty()) {
            record["origin"] = serde_json::Value::String(origin.to_string_lossy().into_owned());
        }
        record
    }
}

/// Milliseconds since the Unix epoch as a `u64`; `0` if the clock predates the epoch.
fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Nanoseconds since the Unix epoch as a `u64` (fits until year ~2262); `0` if the clock predates
/// the epoch (never, in practice).
fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Build the estate [`Node`] for one LIVE event. Id = `<zero-padded-nanos>-<pid>-<seq>` — unique
/// across concurrent emitters AND lexically time-ordered (so an id-ordered scan is a chronological
/// drain). The full envelope rides in `metadata`.
fn event_to_node(event: &EmitEvent, ts_nanos: u64, seq: u64) -> Node {
    let id = format!("{ts_nanos:020}-{}-{seq}", std::process::id());
    node_with_id(event, id, ts_nanos, seq)
}

/// Build the estate [`Node`] for one event under an explicit id (the live path derives it from the
/// clock and a sequence; the replay path from the spool line's content, so it is deterministic).
fn node_with_id(event: &EmitEvent, id: String, ts_nanos: u64, seq: u64) -> Node {
    let mut node = Node::new(
        synthetic_symbol(EVENT, &id),
        NodeKind::Other(EVENT.to_string()),
        event.event_type.clone(),
        Language::new(SYMBOL_SCHEME),
        Location::new(format!("{EVENT}/{id}"), Span::ZERO),
    );
    let m = &mut node.metadata;
    m.insert(
        "event_type".to_string(),
        serde_json::Value::String(event.event_type.clone()),
    );
    m.insert(
        "domain".to_string(),
        serde_json::Value::String(event.domain.clone()),
    );
    m.insert(
        "subdomain".to_string(),
        serde_json::Value::String(event.subdomain.clone()),
    );
    m.insert("payload".to_string(), event.payload.clone());
    m.insert("ts_nanos".to_string(), serde_json::json!(ts_nanos));
    m.insert("seq".to_string(), serde_json::json!(seq));
    node
}

/// Publish `event` onto the shared store through a store handle the caller ALREADY holds — no second
/// connection, no subprocess. PREFER this wherever a store is open.
///
/// Must be called OUTSIDE an open write batch (it opens its own `begin_batch`/`commit_batch`).
/// Fire-and-forget: on a store-write error the event is spooled to the outbox and `false` returned.
pub fn emit_event_to(store: &mut dyn GraphStore, event: &EmitEvent) -> bool {
    let ts = now_nanos();
    let seq = EMIT_SEQ.fetch_add(1, Ordering::Relaxed);
    let node = event_to_node(event, ts, seq);
    match write_event_node(store, node) {
        Ok(()) => true,
        Err(e) => {
            spool(event, &format!("store write failed: {e}"));
            false
        }
    }
}

/// Write one event node through the caller's store via the batch path.
fn write_event_node(store: &mut dyn GraphStore, node: Node) -> anyhow::Result<()> {
    store.begin_batch()?;
    store.upsert_nodes(&[node])?;
    store.commit_batch()?;
    Ok(())
}

/// Publish `event` without a caller-supplied store. Resolves the shared store from [`ESTATE_DB_ENV`]
/// and writes there; if the env is unset/`:memory:` (ephemeral/test scope) the event is appended to
/// the outbox spool instead. NEVER spawns a subprocess.
///
/// Returns `true` if the event was written to the store, `false` if it was spooled. (Callers that
/// already hold a store should use [`emit_event_to`] — it avoids opening a second connection.)
pub fn emit_event(event: &EmitEvent) -> bool {
    match std::env::var(ESTATE_DB_ENV) {
        Ok(p) if !p.is_empty() && p != ":memory:" => match open_store(Some(&p)) {
            Ok(mut store) => emit_event_to(&mut store, event),
            Err(e) => {
                // The error names the store spec; a URL spec may carry credentials, and this text
                // goes to stderr and onto the spool as `deadletter_reason` — redact before either.
                spool(
                    event,
                    &redact_userinfo(&format!("open shared store failed: {e}")),
                );
                false
            }
        },
        _ => {
            spool(event, "no shared store (WICKED_ESTATE_DB unset)");
            false
        }
    }
}

/// Redact the userinfo of every `scheme://user:password@host` URL inside `text` (`scheme://***@host`),
/// so a store spec that carries credentials is never printed to stderr or written into a spool
/// record's `deadletter_reason`. GREEDY on purpose: a raw password may contain `/`, `?` or `#`
/// (`postgres://u:p/a?s#s@h/db`), so the userinfo is taken up to the LAST `@` before the URL ends
/// (whitespace, a quote, `)` or `]` — characters no URL contains); over-redacting a rare `@` in a
/// path (`…/db@x` → `***@x`) is the safe failure. Text without a URL, or a URL without an `@`,
/// is unchanged.
pub fn redact_userinfo(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(scheme_end) = rest.find("://") {
        let (head, tail) = rest.split_at(scheme_end + 3);
        out.push_str(head);
        let url_end = tail
            .find(|c: char| matches!(c, '"' | '\'' | ')' | ']') || c.is_whitespace())
            .unwrap_or(tail.len());
        match tail[..url_end].rfind('@') {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

/// Resolve the home directory cross-platform without external deps: `HOME` (unix) or `USERPROFILE`
/// (Windows).
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Resolve the outbox spool path: the [`DEADLETTER_ENV`] override if set, else
/// `<home>/.something-wicked/wicked-apps/emit-outbox.ndjson`.
///
/// Returns `None` only when no override is set AND the home directory cannot be resolved.
pub fn deadletter_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(DEADLETTER_ENV) {
        return Some(PathBuf::from(p));
    }
    let home = home_dir()?;
    Some(
        home.join(".something-wicked")
            .join("wicked-apps")
            .join("emit-outbox.ndjson"),
    )
}

/// TEST-SUPPORT — never call from runtime code. Redirects the outbox spool to one per-process
/// temp file for the REST of the process, so a test suite that trips a fire-and-forget emission
/// (a gate transition, conformance recording, a rule-lifecycle event) can never append junk to
/// the operator's real `<home>/.something-wicked/wicked-apps/emit-outbox.ndjson` replay queue
/// (core#311).
///
/// One shared helper instead of a copy per test binary. Idempotent (`Once`) and deliberately
/// NEVER unset: an unset window would leak a parallel test's emission to the real spool (the
/// per-test set/remove pattern had exactly that race). Returns the armed spool path — every
/// caller in the same process gets the same file. Spawned subprocesses (e.g. the real
/// `wicked-core` binary under `CARGO_BIN_EXE_*`) inherit the env var, so arming the test process
/// covers its children. Tests that assert spool CONTENTS under their own path must live in a
/// binary that manages [`DEADLETTER_ENV`] itself and must not call this.
///
/// The DEFAULT runtime resolution ([`deadletter_path`]) is unchanged: this only sets the
/// already-honored [`DEADLETTER_ENV`] override, and only in processes that opt in.
///
/// Also arms [`crate::spawn::hermetic_test_worker_home`] — the worker-config-home override is the
/// same test-hygiene guarantee for the engine's ACP spawn path (a real start re-sanitizes the
/// resolved worker home, which must never be the operator's real `~/.wicked-worker/claude`), and
/// riding here means every existing pre-main arming block covers both without edits.
pub fn hermetic_test_spool() -> PathBuf {
    crate::spawn::hermetic_test_worker_home();
    static ARMED: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ARMED
        .get_or_init(|| {
            let path = std::env::temp_dir().join(format!(
                "wicked-apps-test-outbox-{}.ndjson",
                std::process::id()
            ));
            // SAFETY: process-global env write, serialized by `OnceLock` and never removed
            // afterwards, so there is no read-during-unset window to race.
            unsafe { std::env::set_var(DEADLETTER_ENV, &path) };
            path
        })
        .clone()
}

/// Append one NDJSON line for `event` to the outbox spool, writing the loud [`DEADLETTER_MARKER`]
/// lines to stderr. Used whenever the event could not be written to the shared store.
fn spool(event: &EmitEvent, reason: &str) {
    eprintln!(
        "{DEADLETTER_MARKER} event `{}` not stored ({reason}); spooling to outbox",
        event.event_type
    );
    match append_spool(event, reason) {
        Ok(path) => eprintln!(
            "{DEADLETTER_MARKER} spooled `{}` to {}",
            event.event_type,
            path.display()
        ),
        Err(e) => eprintln!(
            "{DEADLETTER_MARKER} FAILED to spool `{}` to outbox: {e}",
            event.event_type
        ),
    }
}

/// Append one NDJSON line for `event` to the outbox spool, creating parent dirs as needed.
fn append_spool(event: &EmitEvent, reason: &str) -> std::io::Result<PathBuf> {
    let path = deadletter_path().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "cannot resolve outbox spool path (no HOME/USERPROFILE and no WICKED_APPS_EMIT_DEADLETTER)",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(&event.spool_record(reason))
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(path)
}

// ─────────────────────────────────────────────────────────────────────────────
// The read side and the drain (wicked-crew#495 / F-022): count what landed, replay what did not.
// ─────────────────────────────────────────────────────────────────────────────

/// EVENT nodes on `store` — the read side of this seam. wicked-crew's `/diagnostics` reports this
/// at boot and live (`records.total` / `records.sinceBoot`) so an operator can see governance
/// evidence LANDING rather than infer it from the absence of dead letters. `GraphRead` has no
/// count primitive, so this loads the event nodes; the governance store is small (thousands, not
/// millions) and the caller caches.
pub fn count_events(store: &dyn GraphRead) -> anyhow::Result<usize> {
    let nodes = store.find_symbols(&SymbolQuery {
        kinds: vec![NodeKind::Other(EVENT.to_string())],
        ..Default::default()
    })?;
    Ok(nodes.len())
}

/// One entry of an outbox that did not land on replay — the ORIGINAL line verbatim (so the caller
/// can keep it dead-lettered without re-serializing) and why.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayFailure {
    pub line: String,
    pub reason: String,
}

/// The outcome of [`replay_outbox`]: non-empty lines read, entries newly written as EVENT nodes,
/// entries an earlier replay had ALREADY landed (their deterministic id was on the store — the
/// re-replay is a no-op), and the entries that did not land (torn lines, records that are not spool
/// records, store write errors).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReplayReport {
    pub read: usize,
    pub replayed: usize,
    #[serde(default)]
    pub already_present: usize,
    pub failed: Vec<ReplayFailure>,
}

/// The deterministic id of a replayed spool line: its original stamp (`ts` millis → nanos; `0`
/// when the record carried none — never the replay clock, which would make the id differ per run)
/// plus the first 16 hex chars of the SHA-256 of the line VERBATIM. The same line replayed twice —
/// a second CLI run on an archive, or the "engine threw → outbox restored whole → run again" path —
/// therefore resolves to the same node id, and `upsert_nodes` makes the second landing a no-op.
fn replay_id(line: &str, ts_nanos: u64) -> String {
    let digest = Sha256::digest(line.as_bytes());
    let mut hex = String::with_capacity(16);
    for byte in &digest[..8] {
        hex.push_str(&format!("{byte:02x}"));
    }
    format!("{ts_nanos:020}-replay-{hex}")
}

/// A spool line parsed back: the envelope, the original spool stamp (`ts` in millis) if the
/// record carried one, and the provenance fields the replayed node keeps.
struct SpoolLine {
    event: EmitEvent,
    ts_millis: Option<u64>,
    reason: Option<String>,
    origin: Option<String>,
}

/// Parse one NDJSON spool line (the shape [`EmitEvent::spool_record`] writes — with or without
/// the `ts`/`pid`/`origin` stamps older engines did not write). Strict on the envelope: a record
/// missing `type`/`domain`/`subdomain` strings or a `payload` object is not an event and is
/// refused with the reason, never guessed at.
fn parse_spool_line(line: &str) -> Result<SpoolLine, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("not a JSON spool record: {e}"))?;
    let obj = v
        .as_object()
        .ok_or_else(|| "spool record is not a JSON object".to_string())?;
    let str_field = |k: &str| -> Result<String, String> {
        obj.get(k)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| format!("spool record has no `{k}` string"))
    };
    let event_type = str_field("type")?;
    let domain = str_field("domain")?;
    let subdomain = str_field("subdomain")?;
    let payload = obj
        .get("payload")
        .filter(|p| p.is_object())
        .cloned()
        .ok_or_else(|| "spool record has no `payload` object".to_string())?;
    Ok(SpoolLine {
        event: EmitEvent::new(event_type, domain, subdomain, payload),
        ts_millis: obj.get("ts").and_then(|t| t.as_u64()),
        reason: obj
            .get("deadletter_reason")
            .and_then(|r| r.as_str())
            .map(str::to_owned),
        origin: obj
            .get("origin")
            .and_then(|o| o.as_str())
            .map(str::to_owned),
    })
}

/// Replay every record in the outbox at `path` onto `store` as the EVENT node it should have
/// been. IDEMPOTENT: the node id is derived from the spool line's content and original stamp
/// ([`replay_id`]), so replaying the same file — or the same line — twice lands nothing twice;
/// lines already on the store are counted as `already_present` (the upsert still runs, so a record
/// a previous replay left half-written is completed rather than skipped). A record's original `ts`
/// (millis) — when it carries one — becomes the node's `ts_nanos` and the id prefix, so the store's
/// id-ordered scan stays chronological; a record without a stamp keeps `ts_nanos: 0` (unknown, not
/// invented) and lands at the front of that order — and, being content-addressed with no stamp to
/// tell them apart, BYTE-IDENTICAL unstamped lines (two pre-stamp dead letters of the same event)
/// share one id and land once; the second is reported `already_present`. Stamped lines never
/// conflate unless their `ts` and content both match. Every replayed node keeps its provenance:
/// `replayed: true`, `replayed_at_ms`, the `deadletter_reason` it was spooled with, and `spooled_by`
/// (the origin) when present. Each record is written on its OWN autocommit statement — no shared
/// write batch — so a failed record leaves no open transaction for the next one to commit into,
/// and the deterministic id lets the next replay repair it. Failures are REPORTED, never re-spooled
/// here (the caller owns the live outbox and decides what to append back); one bad line never stops
/// the rest. Streams the file line by line — never the whole outbox in memory (a host-wide outbox
/// once reached 227 MB); an I/O error mid-file surfaces as the `Err`. The caller is expected to have
/// moved the live outbox aside first so a concurrent emitter's appends are not read half-written.
pub fn replay_outbox(path: &Path, store: &mut dyn GraphStore) -> std::io::Result<ReplayReport> {
    let reader = BufReader::new(std::fs::File::open(path)?);
    let mut report = ReplayReport::default();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        report.read += 1;
        let parsed = match parse_spool_line(&line) {
            Ok(p) => p,
            Err(reason) => {
                report.failed.push(ReplayFailure { line, reason });
                continue;
            }
        };
        let ts = parsed
            .ts_millis
            .map(|ms| ms.saturating_mul(1_000_000))
            .unwrap_or(0);
        let id = replay_id(&line, ts);
        let symbol = synthetic_symbol(EVENT, &id);
        let existed = match store.get_node(&symbol) {
            Ok(found) => found.is_some(),
            Err(e) => {
                report.failed.push(ReplayFailure {
                    line,
                    reason: format!("store read failed: {e}"),
                });
                continue;
            }
        };
        let mut node = node_with_id(&parsed.event, id, ts, 0);
        node.metadata
            .insert("replayed".to_string(), serde_json::Value::Bool(true));
        node.metadata.insert(
            "replayed_at_ms".to_string(),
            serde_json::json!(now_millis()),
        );
        // A LEGACY line (written before the open-failure reason was redacted) can carry a
        // credentialed URL in its reason; it must not become durable store metadata. The line
        // itself stays verbatim in `ReplayFailure` — the caller needs it byte-exact.
        if let Some(reason) = parsed.reason {
            node.metadata.insert(
                "deadletter_reason".to_string(),
                serde_json::Value::String(redact_userinfo(&reason)),
            );
        }
        if let Some(origin) = parsed.origin {
            node.metadata.insert(
                "spooled_by".to_string(),
                serde_json::Value::String(redact_userinfo(&origin)),
            );
        }
        // One autocommit upsert per record — deliberately NOT `write_event_node`'s batch: the
        // `GraphStore` trait has no rollback, so a batch left open by a failed record would be
        // committed by the next record's `commit_batch`.
        match store.upsert_nodes(&[node]) {
            Ok(()) if existed => report.already_present += 1,
            Ok(()) => report.replayed += 1,
            Err(e) => report.failed.push(ReplayFailure {
                line,
                reason: format!("store write failed: {e}"),
            }),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::{
        count_events, deadletter_path, emit_event, emit_event_to, redact_userinfo, replay_id,
        replay_outbox, EmitEvent, DEADLETTER_ENV, ORIGIN_ENV,
    };
    use crate::{GraphRead, NodeKind, SqliteStore, ESTATE_DB_ENV, EVENT, EV_POLICY_EVALUATED};
    use std::sync::{Mutex, MutexGuard};
    use wicked_estate_core::SymbolQuery;

    // `emit_event` reads process-global env vars; serialize the env-mutating tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn read_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        let body = std::fs::read_to_string(path).expect("spool file must exist");
        body.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("each spool line must be valid JSON"))
            .collect()
    }

    /// `emit_event_to` writes the event as an `EVENT` node on the SAME store, queryable by kind,
    /// payload intact — and no subprocess. Falsifier: a write that lands no event node → empty
    /// `find_symbols` → fail.
    #[test]
    fn emit_event_to_writes_an_event_node() {
        let mut store = SqliteStore::in_memory().expect("open in-memory estate store");
        let ev = EmitEvent::new(
            EV_POLICY_EVALUATED,
            "wicked-governance",
            "governance.evaluation",
            serde_json::json!({ "claim_id": "c1", "decision": "allow" }),
        );
        assert!(
            emit_event_to(&mut store, &ev),
            "store-backed emit must report stored"
        );

        let nodes = store
            .find_symbols(&SymbolQuery {
                kinds: vec![NodeKind::Other(EVENT.to_string())],
                ..Default::default()
            })
            .expect("find_symbols ok");
        assert_eq!(nodes.len(), 1, "exactly one event node must be written");
        let n = &nodes[0];
        assert_eq!(n.metadata.get("event_type").unwrap(), EV_POLICY_EVALUATED);
        assert_eq!(n.metadata.get("domain").unwrap(), "wicked-governance");
        assert_eq!(n.metadata.get("payload").unwrap()["claim_id"], "c1");
        assert!(
            n.metadata.get("ts_nanos").unwrap().is_u64(),
            "ts_nanos must be a u64 for chronological ordering"
        );
    }

    /// With no shared store configured, `emit_event` appends a parseable NDJSON line to the outbox
    /// (and never spawns a subprocess). Falsifier: nothing spooled → `read_lines` empty → fail.
    #[test]
    fn emit_event_without_store_spools_to_outbox() {
        let _guard = lock_env();
        let dir = std::env::temp_dir().join(format!("wicked-apps-emit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spool = dir.join("emit-outbox.ndjson");
        let _ = std::fs::remove_file(&spool);

        // SAFETY: env access is serialized by ENV_LOCK; vars are restored before unlock.
        unsafe {
            std::env::set_var(DEADLETTER_ENV, &spool);
            std::env::remove_var(ESTATE_DB_ENV);
        }

        let ev = EmitEvent::new(
            EV_POLICY_EVALUATED,
            "wicked-governance",
            "governance.evaluation",
            serde_json::json!({ "claim_id": "c1" }),
        );
        let stored = emit_event(&ev);

        let lines = read_lines(&spool);

        unsafe {
            std::env::remove_var(DEADLETTER_ENV);
        }
        let _ = std::fs::remove_file(&spool);

        assert!(!stored, "no WICKED_ESTATE_DB ⇒ spooled, not stored");
        assert_eq!(lines.len(), 1, "exactly one NDJSON line must be spooled");
        assert_eq!(lines[0]["type"], EV_POLICY_EVALUATED);
        assert!(
            lines[0]["deadletter_reason"].is_string(),
            "the spooled record records why it was not stored"
        );
    }

    /// Default outbox path is derived from home (cross-platform) and ends with the documented
    /// suffix — never a hardcoded `~`.
    #[test]
    fn default_outbox_path_is_under_home() {
        let _guard = lock_env();
        unsafe {
            std::env::remove_var(DEADLETTER_ENV);
        }
        if let Some(p) = deadletter_path() {
            let s = p.to_string_lossy().replace('\\', "/");
            assert!(
                s.ends_with(".something-wicked/wicked-apps/emit-outbox.ndjson"),
                "unexpected default spool path: {s}"
            );
            assert!(!s.contains('~'), "path must be expanded, not literal ~");
        }
    }

    /// Every spooled record is stamped with WHEN and BY WHOM (crew#495 / F-022): `ts` (epoch
    /// millis), `pid`, and `origin` exactly as the launcher exported it — absent when it did not.
    /// Falsifier: an unstamped record (the pre-fix shape, whose replay order was unrecoverable).
    #[test]
    fn spooled_records_carry_ts_pid_and_origin() {
        let _guard = lock_env();
        let dir = std::env::temp_dir().join(format!("wicked-apps-stamp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spool = dir.join("emit-outbox.ndjson");
        let _ = std::fs::remove_file(&spool);
        unsafe {
            std::env::set_var(DEADLETTER_ENV, &spool);
            std::env::remove_var(ESTATE_DB_ENV);
            std::env::set_var(
                ORIGIN_ENV,
                "wicked-crew@0.7.28 serve pid=4242 port=7701 db=/s/core.db",
            );
        }
        let ev = EmitEvent::new(
            EV_POLICY_EVALUATED,
            "wicked-governance",
            "governance.evaluation",
            serde_json::json!({ "claim_id": "c1" }),
        );
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert!(!emit_event(&ev), "no store ⇒ spooled");
        unsafe {
            std::env::remove_var(ORIGIN_ENV);
        }
        assert!(
            !emit_event(&ev),
            "no store ⇒ spooled (second record, no origin)"
        );
        let lines = read_lines(&spool);
        unsafe {
            std::env::remove_var(DEADLETTER_ENV);
        }
        let _ = std::fs::remove_file(&spool);

        assert_eq!(lines.len(), 2);
        let ts = lines[0]["ts"].as_u64().expect("ts is a u64 (epoch millis)");
        assert!(
            ts >= before && ts < before + 60_000,
            "ts {ts} is not 'now' ({before})"
        );
        assert_eq!(
            lines[0]["pid"].as_u64(),
            Some(u64::from(std::process::id()))
        );
        assert_eq!(
            lines[0]["origin"],
            "wicked-crew@0.7.28 serve pid=4242 port=7701 db=/s/core.db"
        );
        assert!(
            lines[1].get("origin").is_none(),
            "no launcher origin ⇒ no `origin` key, never an invented one"
        );
        assert!(lines[1]["ts"].is_u64());
    }

    /// The drain (crew#495): replaying an outbox lands each valid record as an EVENT node with its
    /// ORIGINAL `ts` restored (chronological id order survives), keeps its dead-letter provenance,
    /// counts through `count_events`, and reports torn / non-record lines VERBATIM instead of
    /// stopping. Falsifier: a torn line that aborts the replay, or a replayed node stamped at
    /// replay time when the record carried its own time.
    #[test]
    fn replay_outbox_lands_records_and_reports_torn_lines() {
        let dir = std::env::temp_dir().join(format!("wicked-apps-replay-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outbox = dir.join("emit-outbox.ndjson");
        let stamped = serde_json::json!({
            "type": "wicked.crew.phase.transitioned",
            "domain": "wicked-crew",
            "subdomain": "crew.phase",
            "payload": { "run": "r1" },
            "deadletter_reason": "no shared store (WICKED_ESTATE_DB unset)",
            "ts": 1_757_500_000_000u64,
            "pid": 4242,
            "origin": "wicked-crew@0.7.28 serve pid=4242 db=/s/core.db"
        });
        let unstamped = serde_json::json!({
            "type": EV_POLICY_EVALUATED,
            "domain": "wicked-governance",
            "subdomain": "governance.evaluation",
            "payload": { "claim_id": "c1" },
            "deadletter_reason": "store write failed: locked"
        });
        let torn = r#"{"type":"wicked.crew.phase.transitioned","domain":"wicked-c"#;
        let not_a_record = r#"{"hello":"world"}"#;
        std::fs::write(
            &outbox,
            format!("{stamped}\n{unstamped}\n\n{torn}\n{not_a_record}\n"),
        )
        .unwrap();

        let mut store = SqliteStore::in_memory().expect("open in-memory estate store");
        assert_eq!(count_events(&store).unwrap(), 0);
        let report = replay_outbox(&outbox, &mut store).expect("outbox readable");
        let _ = std::fs::remove_file(&outbox);

        assert_eq!(report.read, 4, "blank lines are not entries");
        assert_eq!(report.replayed, 2);
        assert_eq!(report.failed.len(), 2);
        assert_eq!(report.failed[0].line, torn);
        assert!(report.failed[0].reason.contains("not a JSON spool record"));
        assert_eq!(report.failed[1].line, not_a_record);
        assert!(report.failed[1].reason.contains("`type`"));
        assert_eq!(count_events(&store).unwrap(), 2);

        let nodes = store
            .find_symbols(&SymbolQuery {
                kinds: vec![NodeKind::Other(EVENT.to_string())],
                ..Default::default()
            })
            .unwrap();
        let phase = nodes
            .iter()
            .find(|n| n.metadata["event_type"] == "wicked.crew.phase.transitioned")
            .expect("the stamped record landed");
        assert_eq!(
            phase.metadata["ts_nanos"].as_u64(),
            Some(1_757_500_000_000u64 * 1_000_000),
            "the ORIGINAL spool time is restored, not the replay time"
        );
        assert_eq!(phase.metadata["replayed"], true);
        assert_eq!(
            phase.metadata["deadletter_reason"],
            "no shared store (WICKED_ESTATE_DB unset)"
        );
        assert_eq!(
            phase.metadata["spooled_by"],
            "wicked-crew@0.7.28 serve pid=4242 db=/s/core.db"
        );
        assert_eq!(phase.metadata["payload"]["run"], "r1");
        let policy = nodes
            .iter()
            .find(|n| n.metadata["event_type"] == EV_POLICY_EVALUATED)
            .expect("the unstamped record landed");
        assert_eq!(
            policy.metadata["ts_nanos"].as_u64(),
            Some(0),
            "an unstamped record keeps an UNKNOWN time (0), never an invented replay time"
        );
        assert!(
            policy.metadata["replayed_at_ms"].as_u64().unwrap() > 1_700_000_000_000,
            "the replay time rides as provenance, not as the record's time"
        );
        assert!(policy.metadata.get("spooled_by").is_none());
        // The serde shape crew parses: `{ read, replayed, already_present, failed: [{ line, reason }] }`.
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["read"], 4);
        assert_eq!(json["replayed"], 2);
        assert_eq!(json["already_present"], 0);
        assert_eq!(json["failed"][0]["line"], torn);
        assert!(json["failed"][0]["reason"].is_string());
    }

    /// IDEMPOTENT replay: the same file replayed twice lands each record ONCE — the replayed id is
    /// the spool line's content hash plus its original stamp, never the replaying pid or a fresh
    /// sequence — and the second run reports every line as `already_present`. Falsifier: the
    /// pre-fix ids (`{ts}-{pid}-{seq}`), under which a restore-and-retry doubled every record.
    #[test]
    fn replaying_the_same_outbox_twice_lands_each_record_once() {
        let dir =
            std::env::temp_dir().join(format!("wicked-apps-replay-twice-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outbox = dir.join("emit-outbox.ndjson");
        let stamped = serde_json::json!({
            "type": "wicked.crew.phase.transitioned",
            "domain": "wicked-crew",
            "subdomain": "crew.phase",
            "payload": { "run": "r1" },
            "deadletter_reason": "no shared store (WICKED_ESTATE_DB unset)",
            "ts": 1_757_500_000_000u64,
            "pid": 4242
        });
        let unstamped = serde_json::json!({
            "type": EV_POLICY_EVALUATED,
            "domain": "wicked-governance",
            "subdomain": "governance.evaluation",
            "payload": { "claim_id": "c1" },
            "deadletter_reason": "store write failed: locked"
        });
        std::fs::write(&outbox, format!("{stamped}\n{unstamped}\n")).unwrap();

        let mut store = SqliteStore::in_memory().expect("open in-memory estate store");
        let first = replay_outbox(&outbox, &mut store).unwrap();
        assert_eq!(
            (first.replayed, first.already_present, first.failed.len()),
            (2, 0, 0)
        );
        assert_eq!(count_events(&store).unwrap(), 2);

        let second = replay_outbox(&outbox, &mut store).unwrap();
        let _ = std::fs::remove_file(&outbox);
        assert_eq!(
            (
                second.read,
                second.replayed,
                second.already_present,
                second.failed.len()
            ),
            (2, 0, 2, 0),
            "a re-replay is a no-op that says so"
        );
        assert_eq!(count_events(&store).unwrap(), 2, "nothing landed twice");

        // The id is a pure function of (stamp, line): same inputs, same id; a different line or a
        // different stamp, a different id.
        let line = stamped.to_string();
        assert_eq!(
            replay_id(&line, 1_757_500_000_000 * 1_000_000),
            replay_id(&line, 1_757_500_000_000 * 1_000_000)
        );
        assert_ne!(replay_id(&line, 1), replay_id(&line, 2));
        assert_ne!(replay_id(&line, 1), replay_id(&unstamped.to_string(), 1));
        assert!(replay_id(&line, 0).starts_with("00000000000000000000-replay-"));
        assert_eq!(replay_id(&line, 0).len(), 20 + "-replay-".len() + 16);
    }

    /// A store spec's credentials never reach stderr or the spool: the "open shared store failed"
    /// reason is redacted before it is printed or written. Falsifier: `s3cret` in the output.
    #[test]
    fn open_failure_reasons_redact_url_userinfo() {
        assert_eq!(
            redact_userinfo(
                "open shared store failed: open estate store at \"postgres://u:s3cret@h:5432/db\": boom"
            ),
            "open shared store failed: open estate store at \"postgres://***@h:5432/db\": boom"
        );
        assert_eq!(
            redact_userinfo("postgresql://user@h/db and mysql://a:b@c/d"),
            "postgresql://***@h/db and mysql://***@c/d"
        );
        assert_eq!(redact_userinfo("postgres://h/db"), "postgres://h/db");
        // GREEDY: a raw password containing `/`, `?` or `#` is still redacted up to the last `@`.
        assert_eq!(
            redact_userinfo("open estate store at \"postgres://u:p/a?s#s@h:5432/db\": boom"),
            "open estate store at \"postgres://***@h:5432/db\": boom"
        );
        assert_eq!(redact_userinfo("mysql://u:p@ss@h/db"), "mysql://***@h/db");
        // Over-redaction of a rare `@` in a path is the safe failure, never a leak.
        assert_eq!(redact_userinfo("postgres://h/db@x"), "postgres://***@x");
        assert_eq!(
            redact_userinfo(
                "open estate store at \"/state/core.db.governance/governance.db\": locked"
            ),
            "open estate store at \"/state/core.db.governance/governance.db\": locked"
        );
        assert_eq!(
            redact_userinfo("no shared store (WICKED_ESTATE_DB unset)"),
            "no shared store (WICKED_ESTATE_DB unset)"
        );
    }

    /// #428 — a LEGACY spool line whose reason (or origin) carries a credentialed URL is redacted
    /// before it becomes durable store metadata; the failure report still carries lines verbatim.
    /// And C6: byte-identical UNSTAMPED lines are content-addressed with no stamp to tell them
    /// apart, so they conflate onto one node (the second is `already_present`).
    #[test]
    fn replay_redacts_legacy_reasons_and_conflates_identical_unstamped_lines() {
        let dir =
            std::env::temp_dir().join(format!("wicked-apps-replay-legacy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let outbox = dir.join("emit-outbox.ndjson");
        let legacy = serde_json::json!({
            "type": EV_POLICY_EVALUATED,
            "domain": "wicked-governance",
            "subdomain": "governance.evaluation",
            "payload": { "claim_id": "c1" },
            "deadletter_reason": "open shared store failed: open estate store at \"postgres://u:s3cret@h/db\": boom",
            "origin": "postgres://u:s3cret@h/db"
        });
        let torn = r#"{"type":"x","domain":"postgres://u:s3cret@h/db"#;
        std::fs::write(&outbox, format!("{legacy}\n{legacy}\n{torn}\n")).unwrap();
        let mut store = SqliteStore::in_memory().expect("open in-memory estate store");
        let report = replay_outbox(&outbox, &mut store).unwrap();
        let _ = std::fs::remove_file(&outbox);
        assert_eq!(
            (
                report.read,
                report.replayed,
                report.already_present,
                report.failed.len()
            ),
            (3, 1, 1, 1),
            "two identical unstamped lines conflate onto one node"
        );
        assert_eq!(count_events(&store).unwrap(), 1);
        let nodes = store
            .find_symbols(&SymbolQuery {
                kinds: vec![NodeKind::Other(EVENT.to_string())],
                ..Default::default()
            })
            .unwrap();
        let stored = serde_json::to_string(&nodes[0].metadata).unwrap();
        assert!(
            !stored.contains("s3cret"),
            "no credential in store metadata: {stored}"
        );
        assert_eq!(
            nodes[0].metadata["deadletter_reason"],
            "open shared store failed: open estate store at \"postgres://***@h/db\": boom"
        );
        assert_eq!(nodes[0].metadata["spooled_by"], "postgres://***@h/db");
        // The failure report keeps the line VERBATIM — the caller must be able to re-spool it byte-exact.
        assert_eq!(report.failed[0].line, torn);
    }
}
